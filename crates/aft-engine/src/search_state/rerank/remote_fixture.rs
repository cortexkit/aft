use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

pub fn serve(
    body: &'static str,
    status: u16,
    delay: Duration,
) -> (String, std::thread::JoinHandle<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let body_start;
        let length;
        loop {
            let mut buf = [0; 1024];
            let read = stream.read(&mut buf).unwrap();
            assert!(read > 0);
            bytes.extend_from_slice(&buf[..read]);
            if let Some(start) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                body_start = start + 4;
                let headers = String::from_utf8_lossy(&bytes[..start]);
                assert!(headers.starts_with("POST /rerank HTTP/1.1"));
                length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|value| value.parse::<usize>().unwrap())
                    })
                    .unwrap();
                break;
            }
        }
        while bytes.len() < body_start + length {
            let mut buf = [0; 1024];
            let read = stream.read(&mut buf).unwrap();
            assert!(read > 0);
            bytes.extend_from_slice(&buf[..read]);
        }
        let request = serde_json::from_slice(&bytes[body_start..body_start + length]).unwrap();
        std::thread::sleep(delay);
        let _ = write!(stream, "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        request
    });
    (endpoint, thread)
}
