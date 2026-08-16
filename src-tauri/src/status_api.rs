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
    let mut request = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 2048];

    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                request.extend_from_slice(&chunk[..read]);
                if request.len() >= 8192 || request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => {
                eprintln!("[STATUS API] request read failed: {error}");
                return;
            }
        }
    }

    let request_text = String::from_utf8_lossy(&request);
    let request_line = request_text.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let Some(method) = parts.next() else {
        send_json(
            stream,
            "400 Bad Request",
            b"{\"error\":\"bad_request\"}",
            None,
        );
        return;
    };
    let Some(raw_path) = parts.next() else {
        send_json(
            stream,
            "400 Bad Request",
            b"{\"error\":\"bad_request\"}",
            None,
        );
        return;
    };

    if method != "GET" {
        send_json(
            stream,
            "405 Method Not Allowed",
            b"{\"error\":\"method_not_allowed\"}",
            Some("Allow: GET\r\n"),
        );
        return;
    }

    let path = raw_path.split('?').next().unwrap_or(raw_path);
    if path != "/v1/status" {
        send_json(stream, "404 Not Found", b"{\"error\":\"not_found\"}", None);
        return;
    }

    match crate::read_status_api_json(app) {
        Ok(body) => send_json(stream, "200 OK", &body, None),
        Err(error) => {
            eprintln!("[STATUS API] status serialization failed: {error}");
            send_json(
                stream,
                "500 Internal Server Error",
                b"{\"error\":\"status_unavailable\"}",
                None,
            );
        }
    }
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
