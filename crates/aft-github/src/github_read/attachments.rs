use std::io::Read;
use std::sync::LazyLock;
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::header::{CONTENT_TYPE, LOCATION};
use reqwest::redirect::Policy;
use url::Url;

/// At most this many GitHub-hosted images become attachments for one read.
/// Keeping the count fixed prevents a long issue thread from creating an
/// unbounded transport payload.
pub const MAX_GITHUB_IMAGE_ATTACHMENTS: usize = 8;
/// Attachments for one read may contain at most this many downloaded bytes in
/// total. A candidate that exceeds the remaining budget is dropped whole, so
/// callers never receive a partial image.
pub const MAX_GITHUB_IMAGE_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_REDIRECTS: usize = 5;
const IMAGE_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IMAGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

static IMAGE_URL: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"https://[^\s<>\"'()\[\]]+"#)
        .expect("image URL regular expression is valid")
});

/// One complete, validated image attachment. Text rendering retains the source
/// URL; this is out-of-band data for a caller that explicitly supports vision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubImageAttachment {
    pub source_url: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// A downloader result whose final URL is retained for redirect validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadedGithubImage {
    pub final_url: Url,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Synchronous downloader used inside a deferred worker. Tests can replace it
/// with a deterministic fixture downloader that records URLs and byte budgets.
pub trait GithubImageDownloader: Send + Sync {
    fn download(
        &self,
        url: &Url,
        maximum_bytes: usize,
    ) -> Result<Option<DownloadedGithubImage>, String>;
}

/// Production GitHub-image downloader. Redirect following is manual so every
/// target receives the same allowlist check as the original URL.
#[derive(Default)]
pub struct ReqwestGithubImageDownloader;

impl GithubImageDownloader for ReqwestGithubImageDownloader {
    fn download(
        &self,
        url: &Url,
        maximum_bytes: usize,
    ) -> Result<Option<DownloadedGithubImage>, String> {
        if !is_allowed_github_image_url(url) || maximum_bytes == 0 {
            return Ok(None);
        }
        let tls = crate::platform_tls::client_config()
            .map_err(|error| format!("failed to configure GitHub image TLS: {error}"))?;
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(IMAGE_CONNECT_TIMEOUT)
            .timeout(IMAGE_REQUEST_TIMEOUT)
            .use_preconfigured_tls(tls)
            .build()
            .map_err(|error| format!("failed to build GitHub image client: {error}"))?;
        let mut current = url.clone();

        for redirects in 0..=MAX_IMAGE_REDIRECTS {
            if !is_allowed_github_image_url(&current) {
                return Ok(None);
            }
            let mut response = client
                .get(current.clone())
                .send()
                .map_err(|error| format!("image download failed: {error}"))?;
            if response.status().is_redirection() {
                if redirects == MAX_IMAGE_REDIRECTS {
                    return Ok(None);
                }
                let Some(location) = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                else {
                    return Ok(None);
                };
                let next = current.join(location).map_err(|error| {
                    format!("GitHub image redirect location is invalid: {error}")
                })?;
                if !is_allowed_github_image_url(&next) {
                    return Ok(None);
                }
                current = next;
                continue;
            }
            if !response.status().is_success() {
                return Ok(None);
            }
            let mime = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(|value| {
                    value
                        .split(';')
                        .next()
                        .unwrap_or(value)
                        .trim()
                        .to_ascii_lowercase()
                })
                .filter(|value| supported_image_mime(value))
                .ok_or_else(|| "GitHub image response had an unsupported media type".to_string())?;
            if response
                .content_length()
                .is_some_and(|length| length > maximum_bytes as u64)
            {
                return Ok(None);
            }
            let mut bytes = Vec::new();
            let mut limited = response
                .by_ref()
                .take(maximum_bytes.saturating_add(1) as u64);
            limited
                .read_to_end(&mut bytes)
                .map_err(|error| format!("failed to read GitHub image: {error}"))?;
            if bytes.len() > maximum_bytes {
                return Ok(None);
            }
            return Ok(Some(DownloadedGithubImage {
                final_url: current,
                mime,
                bytes,
            }));
        }
        Ok(None)
    }
}

/// Discover eligible HTTPS GitHub image candidates in textual document order.
/// Repeated URLs remain repeated candidates: their positions in the canonical
/// document determine which attachment claims a count slot first.
pub fn discover_github_image_urls(canonical_text: &str) -> Vec<Url> {
    IMAGE_URL
        .find_iter(canonical_text)
        .filter_map(|capture| {
            let raw = capture.as_str().trim_end_matches(|character: char| {
                matches!(character, '.' | ',' | ';' | ':' | '!' | '?')
            });
            Url::parse(raw).ok()
        })
        .filter(is_allowed_github_image_url)
        .collect()
}

/// Download complete attachment candidates without changing canonical text.
/// This function is capability-agnostic so tests can call it directly; callers
/// with a missing or false vision capability must not invoke it.
pub fn download_github_image_attachments(
    canonical_text: &str,
    downloader: &dyn GithubImageDownloader,
) -> Vec<GithubImageAttachment> {
    let mut attachments = Vec::new();
    let mut consumed_bytes = 0usize;
    for url in discover_github_image_urls(canonical_text) {
        if attachments.len() == MAX_GITHUB_IMAGE_ATTACHMENTS {
            break;
        }
        let remaining = MAX_GITHUB_IMAGE_ATTACHMENT_BYTES.saturating_sub(consumed_bytes);
        if remaining == 0 {
            break;
        }
        let Ok(Some(downloaded)) = downloader.download(&url, remaining) else {
            continue;
        };
        // Fixture downloaders are untrusted too: they must not let tests or a
        // future implementation bypass final redirect verification or budgets.
        if !is_allowed_github_image_url(&downloaded.final_url)
            || !supported_image_mime(&downloaded.mime)
            || !supported_image_bytes(&downloaded.bytes)
            || downloaded.bytes.len() > remaining
        {
            continue;
        }
        consumed_bytes += downloaded.bytes.len();
        attachments.push(GithubImageAttachment {
            source_url: url.to_string(),
            mime: downloaded.mime,
            bytes: downloaded.bytes,
        });
    }
    attachments
}

/// The only hosts allowed to become vision attachments. `github.com` is
/// limited to `/user-attachments/`; an arbitrary GitHub page is not an image
/// source and must not turn this feature into a generic web fetcher.
pub fn is_allowed_github_image_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.port(), None | Some(443))
        && match url.host_str() {
            Some("user-images.githubusercontent.com") => true,
            Some("github.com") => url.path().starts_with("/user-attachments/"),
            _ => false,
        }
}

fn supported_image_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

fn supported_image_bytes(bytes: &[u8]) -> bool {
    matches!(
        image::guess_format(bytes),
        Ok(image::ImageFormat::Png
            | image::ImageFormat::Jpeg
            | image::ImageFormat::Gif
            | image::ImageFormat::WebP)
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct FixtureDownloader {
        calls: Mutex<Vec<(String, usize)>>,
    }

    impl GithubImageDownloader for FixtureDownloader {
        fn download(
            &self,
            url: &Url,
            maximum_bytes: usize,
        ) -> Result<Option<DownloadedGithubImage>, String> {
            self.calls
                .lock()
                .unwrap()
                .push((url.to_string(), maximum_bytes));
            Ok(Some(DownloadedGithubImage {
                final_url: url.clone(),
                mime: "image/png".to_string(),
                bytes: vec![137, 80, 78, 71, 13, 10, 26, 10],
            }))
        }
    }

    #[test]
    fn attachment_urls_are_allowlisted_in_document_order() {
        let source = concat!(
            "https://example.test/nope.png ",
            "https://github.com/user-attachments/files/1/a.png ",
            "https://user-images.githubusercontent.com/2/b.png"
        );
        let urls = discover_github_image_urls(source);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].host_str(), Some("github.com"));
        assert_eq!(
            urls[1].host_str(),
            Some("user-images.githubusercontent.com")
        );
    }

    #[test]
    fn downloader_receives_only_complete_allowlisted_attachments() {
        let downloader = FixtureDownloader::default();
        let attachments = download_github_image_attachments(
            "https://user-images.githubusercontent.com/2/b.png",
            &downloader,
        );
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].bytes, vec![137, 80, 78, 71, 13, 10, 26, 10]);
        assert_eq!(downloader.calls.lock().unwrap().len(), 1);
    }
}
