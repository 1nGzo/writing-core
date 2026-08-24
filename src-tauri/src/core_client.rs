use std::{
    io::{Read, Write},
    net::{Shutdown, TcpStream},
    time::Duration,
};

use serde::de::DeserializeOwned;
use serde::Serialize;

const CORE_ADDRESS: &str = "127.0.0.1:43180";
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

pub fn request_json<T, B>(method: &str, path: &str, body: Option<&B>) -> Result<T, String>
where
    T: DeserializeOwned,
    B: Serialize + ?Sized,
{
    let body = body
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|error| format!("Cannot encode Writing Core request: {error}"))?
        .unwrap_or_default();
    let mut stream = TcpStream::connect_timeout(
        &CORE_ADDRESS
            .parse()
            .map_err(|error| format!("Invalid Writing Core address: {error}"))?,
        Duration::from_millis(800),
    )
    .map_err(|error| format!("Writing Core 不可用: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_millis(1200)))
        .map_err(|error| format!("Cannot configure Writing Core connection: {error}"))?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:43180\r\nAccept: application/json\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.write_all(&body))
        .map_err(|error| format!("Cannot send Writing Core request: {error}"))?;
    let _ = stream.shutdown(Shutdown::Write);

    let mut response = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES as u64)
        .read_to_end(&mut response)
        .map_err(|error| format!("Cannot read Writing Core response: {error}"))?;
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .ok_or_else(|| "Writing Core returned an invalid HTTP response".to_string())?;
    let status_line = std::str::from_utf8(&response[..header_end])
        .ok()
        .and_then(|headers| headers.lines().next())
        .unwrap_or_default();
    if !status_line.contains(" 200 ") {
        let message = serde_json::from_slice::<serde_json::Value>(&response[header_end..])
            .ok()
            .and_then(|value| value.get("error")?.as_str().map(str::to_string))
            .unwrap_or_else(|| status_line.to_string());
        return Err(format!("Writing Core 请求失败: {message}"));
    }
    serde_json::from_slice(&response[header_end..])
        .map_err(|error| format!("Writing Core returned invalid JSON: {error}"))
}
