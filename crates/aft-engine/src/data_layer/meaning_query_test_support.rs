use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

const DIMENSIONS: usize = 8;

/// Every text's vector follows from its digest.
fn vector_of(text: &str) -> Vec<f32> {
    let hash = blake3::hash(text.as_bytes());
    (0..DIMENSIONS)
        .map(|i| (f32::from(hash.as_bytes()[i]) - 127.5) / 50.0)
        .collect()
}

/// Holds every embedding request whose input names `marker` until
/// [`Gate::open`] is called.
pub struct Gate {
    marker: &'static str,
    open: Mutex<bool>,
    opened: Condvar,
    held: AtomicUsize,
    /// Texts embedded, excluding the semantic index's model fingerprint probe.
    pub(crate) texts: AtomicUsize,
}

impl Gate {
    pub fn held(&self) -> usize {
        self.held.load(Ordering::SeqCst)
    }

    pub fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_all();
    }

    #[cfg(feature = "test-support")]
    pub fn texts_embedded(&self) -> usize {
        self.texts.load(Ordering::SeqCst)
    }

    fn wait_if_held(&self, texts: &[String]) {
        if !texts.iter().any(|text| text.contains(self.marker)) {
            return;
        }
        self.held.fetch_add(1, Ordering::SeqCst);
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.opened.wait(open).unwrap();
        }
    }
}

/// A loopback OpenAI-compatible embedding server answering every text
/// with [`vector_of`], holding requests that name `marker` until the
/// returned gate opens. Returns its base URL.
pub fn serve(marker: &'static str) -> (String, Arc<Gate>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let gate = Arc::new(Gate {
        marker,
        open: Mutex::new(false),
        opened: Condvar::new(),
        held: AtomicUsize::new(0),
        texts: AtomicUsize::new(0),
    });
    let served = Arc::clone(&gate);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                continue;
            };
            let gate = Arc::clone(&served);
            std::thread::spawn(move || {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 8192];
                let (header_end, length) = loop {
                    let read = stream.read(&mut chunk).unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&buffer[..end]).to_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (end + 4, length);
                    }
                };
                while buffer.len() < header_end + length {
                    let read = stream.read(&mut chunk).unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                }
                let request: Value =
                    serde_json::from_slice(&buffer[header_end..header_end + length])
                        .unwrap_or(Value::Null);
                let texts: Vec<String> = match &request["input"] {
                    Value::String(text) => vec![text.clone()],
                    Value::Array(items) => items
                        .iter()
                        .map(|item| item.as_str().unwrap_or_default().to_string())
                        .collect(),
                    _ => Vec::new(),
                };
                gate.texts.fetch_add(
                    texts
                        .iter()
                        .filter(|text| *text != "semantic index fingerprint probe")
                        .count(),
                    Ordering::SeqCst,
                );
                gate.wait_if_held(&texts);
                let data: Vec<Value> = texts
                    .iter()
                    .enumerate()
                    .map(|(index, text)| json!({"index": index, "embedding": vector_of(text)}))
                    .collect();
                let body = json!({"object": "list", "data": data}).to_string();
                let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                let _ = stream.write_all(reply.as_bytes());
            });
        }
    });
    (format!("http://{address}/v1"), gate)
}
