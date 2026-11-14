use std::io::Read;
use std::time::{Duration, Instant};

use super::GrammarManifest;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadFailure {
    Network,
    Verification,
}

pub struct Response {
    pub status: u16,
    pub location: Option<String>,
    pub body: Box<dyn Read + Send>,
}

/// A hop, rather than an entire download, is injectable so redirect and body rules
/// are exercised by the same code in production and deterministic tests.
pub trait Transport: Send + Sync {
    fn get(&self, url: &reqwest::Url, remaining: Duration) -> Result<Response, DownloadFailure>;
}

pub struct HttpsTransport {
    client: reqwest::blocking::Client,
}

impl HttpsTransport {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .use_preconfigured_tls(crate::platform_tls::client_config()?)
                .build()
                .map_err(|e| e.to_string())?,
        })
    }
}

impl Transport for HttpsTransport {
    fn get(&self, url: &reqwest::Url, remaining: Duration) -> Result<Response, DownloadFailure> {
        let response = self
            .client
            .get(url.clone())
            .timeout(remaining)
            .send()
            .map_err(|_| DownloadFailure::Network)?;
        Ok(Response {
            status: response.status().as_u16(),
            location: response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|h| h.to_str().ok())
                .map(str::to_owned),
            body: Box::new(response),
        })
    }
}

pub fn attempt_deadline(size: u64) -> Duration {
    Duration::from_secs(30).saturating_add(Duration::from_secs_f64(size as f64 / 65536.0))
}

pub fn fetch(
    manifest: &GrammarManifest,
    transport: &dyn Transport,
) -> Result<Vec<u8>, DownloadFailure> {
    let start = Instant::now();
    fetch_timed(manifest, transport, &|| start.elapsed())
}

pub(super) fn fetch_timed(
    manifest: &GrammarManifest,
    transport: &dyn Transport,
    elapsed: &dyn Fn() -> Duration,
) -> Result<Vec<u8>, DownloadFailure> {
    let mut url = reqwest::Url::parse(&manifest.url).map_err(|_| DownloadFailure::Network)?;
    let deadline = attempt_deadline(manifest.size);
    for hop in 0..=5 {
        if url.scheme() != "https" {
            return Err(DownloadFailure::Network);
        }
        let remaining = deadline
            .checked_sub(elapsed())
            .filter(|d| !d.is_zero())
            .ok_or(DownloadFailure::Network)?;
        let mut response = transport.get(&url, remaining)?;
        if (300..400).contains(&response.status) {
            if hop == 5 {
                return Err(DownloadFailure::Network);
            }
            url = url
                .join(
                    response
                        .location
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .ok_or(DownloadFailure::Network)?,
                )
                .map_err(|_| DownloadFailure::Network)?;
            continue;
        }
        if !(200..300).contains(&response.status) {
            return Err(DownloadFailure::Network);
        }
        // Read at most size + 1 bytes, even when Content-Length is absent or false.
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            if elapsed() >= deadline {
                return Err(DownloadFailure::Network);
            }
            let limit = (manifest
                .size
                .saturating_add(1)
                .saturating_sub(bytes.len() as u64))
            .min(buffer.len() as u64) as usize;
            let n = response
                .body
                .read(&mut buffer[..limit])
                .map_err(|_| DownloadFailure::Network)?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..n]);
            if bytes.len() as u64 > manifest.size {
                return Err(DownloadFailure::Verification);
            }
        }
        if elapsed() >= deadline {
            return Err(DownloadFailure::Network);
        }
        return if super::storage::verified(manifest, &bytes) {
            Ok(bytes)
        } else {
            Err(DownloadFailure::Verification)
        };
    }
    Err(DownloadFailure::Network)
}
