use std::{
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use tauri::AppHandle;

pub const STATUS_API_PORT: u16 = 43180;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

pub struct StatusApiRuntime {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl StatusApiRuntime {
    pub fn start(app: AppHandle) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let listener = match TcpListener::bind(("127.0.0.1", STATUS_API_PORT)) {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!(
                    "[STATUS API] unavailable: cannot bind 127.0.0.1:{STATUS_API_PORT}: {error}"
                );
                return Self { stop, join: None };
            }
        };

        if let Err(error) = listener.set_nonblocking(true) {
            eprintln!(
                "[STATUS API] unavailable: cannot configure 127.0.0.1:{STATUS_API_PORT}: {error}"
            );
            return Self { stop, join: None };
        }

        let thread_stop = Arc::clone(&stop);
        let join = match thread::Builder::new()
            .name("writing-status-api".to_string())
            .spawn(move || run_server(listener, app, thread_stop))
        {
            Ok(join) => Some(join),
            Err(error) => {
                eprintln!("[STATUS API] unavailable: cannot start server thread: {error}");
                None
            }
        };

        if join.is_some() {
            eprintln!("[STATUS API] listening on http://127.0.0.1:{STATUS_API_PORT}/v1/status");
        }

        Self { stop, join }
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            if join.join().is_err() {
                eprintln!("[STATUS API] server thread terminated with a panic");
            }
        }
    }
}

impl Drop for StatusApiRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_server(listener: TcpListener, app: AppHandle, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _peer)) => handle_connection(&mut stream, &app),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => {
                eprintln!("[STATUS API] accept failed: {error}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn handle_connection(stream: &mut TcpStream, app: &AppHandle) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let request = match read_http_request(stream) {
        Ok(request) => request,
        Err((status, code)) => {
            send_error(stream, status, code, None);
            return;
        }
    };
    let path = request.path.split('?').next().unwrap_or(&request.path);

    let result = match (request.method.as_str(), path) {
        ("GET", "/v1/status") => crate::read_status_api_json(app),
        ("POST", "/v1/track") => serde_json::from_slice::<crate::TrackApiRequest>(&request.body)
            .map_err(|_| "invalid track request".to_string())
            .and_then(|payload| crate::track_document_api_json(app, payload)),
        ("POST", "/v1/snapshot") => {
            serde_json::from_slice::<crate::LiveSnapshotApiRequest>(&request.body)
                .map_err(|_| "invalid snapshot request".to_string())
                .and_then(|payload| crate::ingest_live_snapshot_api_json(app, payload))
        }
        ("GET" | "POST", _) => {
            send_error(stream, "404 Not Found", "not_found", None);
            return;
        }
        _ => {
            send_error(
                stream,
                "405 Method Not Allowed",
                "method_not_allowed",
                Some("Allow: GET, POST\r\n"),
            );
            return;
        }
    };

    match result {
        Ok(body) => send_json(stream, "200 OK", &body, None),
        Err(error) => {
            let (status, code) = if error.contains("tracked document") {
                ("409 Conflict", "tracked_document_mismatch")
            } else if error.contains("limit") {
                ("413 Payload Too Large", "snapshot_too_large")
            } else {
                ("400 Bad Request", "invalid_request")
            };
            eprintln!("[STATUS API] request rejected path={path} error={error}");
            send_error(stream, status, code, None);
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, (&'static str, &'static str)> {
    let mut request = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        if request.len() >= MAX_HEADER_BYTES {
            return Err(("431 Request Header Fields Too Large", "headers_too_large"));
        }
        let read = stream
            .read(&mut chunk)
            .map_err(|_| ("400 Bad Request", "bad_request"))?;
        if read == 0 {
            return Err(("400 Bad Request", "bad_request"));
        }
        request.extend_from_slice(&chunk[..read]);
    };

    let headers = std::str::from_utf8(&request[..header_end])
        .map_err(|_| ("400 Bad Request", "bad_request"))?;
    let mut lines = headers.split("\r\n");
    let mut request_parts = lines.next().unwrap_or_default().split_whitespace();
    let method = request_parts
        .next()
        .ok_or(("400 Bad Request", "bad_request"))?
        .to_string();
    let path = request_parts
        .next()
        .ok_or(("400 Bad Request", "bad_request"))?
        .to_string();
    let content_length = lines
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return Err(("413 Payload Too Large", "payload_too_large"));
    }
    let expected = header_end + content_length;
    while request.len() < expected {
        let read = stream
            .read(&mut chunk)
            .map_err(|_| ("400 Bad Request", "incomplete_body"))?;
        if read == 0 {
            return Err(("400 Bad Request", "incomplete_body"));
        }
        request.extend_from_slice(&chunk[..read]);
        if request.len() > MAX_HEADER_BYTES + MAX_BODY_BYTES {
            return Err(("413 Payload Too Large", "payload_too_large"));
        }
    }

    Ok(HttpRequest {
        method,
        path,
        body: request[header_end..expected].to_vec(),
    })
}

fn send_error(stream: &mut TcpStream, status: &str, code: &str, extra_headers: Option<&str>) {
    let body = serde_json::to_vec(&serde_json::json!({ "error": code }))
        .unwrap_or_else(|_| b"{\"error\":\"internal_error\"}".to_vec());
    send_json(stream, status, &body, extra_headers);
}

fn send_json(stream: &mut TcpStream, status: &str, body: &[u8], extra_headers: Option<&str>) {
    let extra_headers = extra_headers.unwrap_or_default();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
        body.len()
    );
    if let Err(error) = stream
        .write_all(response.as_bytes())
        .and_then(|_| stream.write_all(body))
    {
        eprintln!("[STATUS API] response write failed: {error}");
    }
    let _ = stream.shutdown(Shutdown::Both);
}
