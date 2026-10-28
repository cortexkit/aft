//! Bounded previews of remote documents. A partial JSON download is not a
//! complete JSON document; scan top-level member spans without inventing values
//! for keys that have not arrived yet.

use std::path::Path;

use crate::protocol::{RawRequest, Response};
use crate::url_fetch::{download_notice, is_http_url};

pub(crate) const URL_OUTPUT_BYTES: usize = 50 * 1024;

pub(crate) fn cap_text(text: &mut String, ceiling: usize, narrow: &str) {
    let total = text.len();
    if total <= ceiling {
        return;
    }
    // Reserve the complete footer before cutting and retreat to a UTF-8
    // boundary. The reported N counts retained source bytes, not the footer.
    let footer_budget =
        format!("\ntruncated at {ceiling} of {total} bytes; narrow with {narrow}").len();
    let mut end = ceiling.saturating_sub(footer_budget);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(&format!(
        "\ntruncated at {end} of {total} bytes; narrow with {narrow}"
    ));
}

pub(crate) fn request_has_url(req: &RawRequest) -> bool {
    ["file", "url", "target"].iter().any(|key| {
        req.params
            .get(*key)
            .and_then(|v| v.as_str())
            .is_some_and(is_http_url)
    }) || req
        .params
        .get("files")
        .and_then(|value| value.as_array())
        .is_some_and(|files| {
            files
                .iter()
                .filter_map(|file| file.as_str())
                .any(is_http_url)
        })
        || req
            .params
            .get("targets")
            .and_then(|v| v.as_array())
            .is_some_and(|targets| {
                targets.iter().any(|target| {
                    ["file", "path", "url"].iter().any(|key| {
                        target
                            .get(*key)
                            .and_then(|v| v.as_str())
                            .is_some_and(is_http_url)
                    })
                })
            })
}

/// Replace cached-document paths before a URL response is shown to an agent.
/// The cache location is private; the caller already has the URL.
pub(crate) fn redact_url_cache_paths(
    req: &RawRequest,
    ctx: &crate::context::AppContext,
    response: &mut Response,
) {
    if !request_has_url(req) {
        return;
    }
    // Downloads are written under the cache root (`AppContext::cache_dir`),
    // which is the `storage.cache_dir` volume when one is set; downloads from
    // before that setting stay under the durable storage root. Redact both.
    let config = ctx.config();
    let mut roots = vec![ctx.cache_dir()];
    if let Some(configured) = &config.cache_dir_configured {
        roots.push(configured.spelling.join("aft"));
    }
    roots.push(crate::bash_background::storage_dir(
        config.storage_dir.as_deref(),
    ));
    roots.dedup();
    for root in roots {
        // A placeholder fetch's private document lives beside the URL cache
        // and is redacted the same way.
        for folder in ["url_cache", crate::url_fetch::SECRET_FETCH_DIR] {
            let cache_dir = root.join(folder).to_string_lossy().into_owned();
            redact_cache_paths_in_value(&mut response.data, &cache_dir);
        }
    }
}

fn redact_cache_paths_in_value(value: &mut serde_json::Value, cache_dir: &str) {
    match value {
        serde_json::Value::String(text) => redact_cache_paths_in_text(text, cache_dir),
        serde_json::Value::Array(items) => {
            for item in items {
                redact_cache_paths_in_value(item, cache_dir);
            }
        }
        serde_json::Value::Object(fields) => {
            for item in fields.values_mut() {
                redact_cache_paths_in_value(item, cache_dir);
            }
        }
        _ => {}
    }
}

fn redact_cache_paths_in_text(text: &mut String, cache_dir: &str) {
    if cache_dir.is_empty() || !text.contains(cache_dir) {
        return;
    }
    let mut redacted = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(start) = rest.find(cache_dir) {
        redacted.push_str(&rest[..start]);
        let mut end = start + cache_dir.len();
        let suffix = &rest[end..];
        if suffix.starts_with('/') || suffix.starts_with('\\') {
            let after_separator = &suffix[1..];
            let filename_end = after_separator
                .char_indices()
                .find(|(_, ch)| ch.is_whitespace() || ",;:)]}".contains(*ch))
                .map(|(offset, _)| offset)
                .unwrap_or(after_separator.len());
            end += 1 + filename_end;
        }
        redacted.push_str("the requested URL");
        rest = &rest[end..];
    }
    redacted.push_str(rest);
    *text = redacted;
}

/// Cap the rendered aggregate, not each symbol separately. This also bounds
/// multi-symbol and mixed local/URL batches and the formatter's line gutters.
pub(crate) fn cap_zoom_response(
    req: &RawRequest,
    ctx: &crate::context::AppContext,
    mut response: Response,
) -> Response {
    redact_url_cache_paths(req, ctx, &mut response);
    if !request_has_url(req) {
        return response;
    }
    let ctx = crate::subc_format::FormatContext {
        zoom_target_label: req
            .params
            .get("file")
            .or_else(|| req.params.get("url"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        ..Default::default()
    };
    let mut text = crate::subc_format::format_response_unbounded("zoom", &response, &ctx);
    if text.len() > URL_OUTPUT_BYTES {
        cap_text(
            &mut text,
            URL_OUTPUT_BYTES,
            "one symbol, a smaller section, or a filtered URL",
        );
        if response.success {
            // Hosts authorize the source manifest before releasing text. Keep
            // it and per-result metadata even when the visible aggregate is cut.
            response.data["text"] = text.clone().into();
            response.data["content"] = text.into();
            response.data["complete"] = false.into();
        } else {
            response.data["message"] = text.into();
        }
    }
    response
}

pub(crate) fn disclose_download(path: &Path, response: &mut Response) {
    if let Some(notice) = download_notice(path) {
        let key = if response.success {
            "content"
        } else {
            "message"
        };
        if let Some(text) = response.data.get_mut(key) {
            if let Some(content) = text.as_str() {
                *text = format!("{notice}\n\n{content}").into();
            }
        }
        response.data["complete"] = false.into();
    }
}

struct Member<'a> {
    name: String,
    value: &'a str,
    complete: bool,
}

fn string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

fn skip_space(bytes: &[u8], i: &mut usize) {
    while *i < bytes.len() && bytes[*i].is_ascii_whitespace() {
        *i += 1;
    }
}

fn top_level_members(source: &str) -> Vec<Member<'_>> {
    let bytes = source.as_bytes();
    let mut i = 0;
    skip_space(bytes, &mut i);
    if bytes.get(i) != Some(&b'{') {
        return Vec::new();
    }
    i += 1;
    let mut members = Vec::new();
    loop {
        skip_space(bytes, &mut i);
        if bytes.get(i) != Some(&b'"') {
            break;
        }
        let Some(end) = string_end(bytes, i) else {
            break;
        };
        let Ok(name) = serde_json::from_str::<String>(&source[i..end]) else {
            break;
        };
        i = end;
        skip_space(bytes, &mut i);
        if bytes.get(i) != Some(&b':') {
            break;
        }
        i += 1;
        skip_space(bytes, &mut i);
        let start = i;
        let mut depth = 0usize;
        let mut complete = true;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => match string_end(bytes, i) {
                    Some(end) => {
                        i = end;
                        continue;
                    }
                    None => {
                        i = bytes.len();
                        complete = false;
                        break;
                    }
                },
                b'{' | b'[' => depth += 1,
                b'}' | b']' if depth > 0 => depth -= 1,
                b',' | b'}' if depth == 0 => break,
                _ => {}
            }
            i += 1;
        }
        complete &= i < bytes.len() && depth == 0;
        members.push(Member {
            name,
            value: source[start..i].trim_end(),
            complete,
        });
        if bytes.get(i) != Some(&b',') {
            break;
        }
        i += 1;
    }
    members
}

pub(crate) fn json_preview(req: &RawRequest, source: &str, symbol: Option<&str>) -> Response {
    let members = top_level_members(source);
    if let Some(symbol) = symbol {
        if let Some(member) = members.iter().find(|m| m.name == symbol) {
            let mut content = member.value.to_string();
            if !member.complete {
                content.insert_str(
                    0,
                    "Partial value from downloaded prefix (not complete JSON):\n",
                );
            }
            let complete = member.complete && content.len() <= URL_OUTPUT_BYTES - 1024;
            cap_text(
                &mut content,
                URL_OUTPUT_BYTES - 1024,
                "a filtered URL selecting a smaller value",
            );
            return Response::success(
                &req.id,
                serde_json::json!({
                    "name": symbol, "kind": "json_value", "content": content,
                    "complete": complete,
                }),
            );
        }
    }
    let mut text = match symbol {
        Some(symbol) => format!("Top-level key {symbol:?} not found in downloaded prefix.\n"),
        None => String::new(),
    };
    text.push_str("JSON top-level keys and value sizes in downloaded prefix:\n");
    if members.is_empty() {
        text.push_str(
            "No top-level object keys available (array, scalar, or incomplete object).\n",
        );
    }
    for member in members {
        text.push_str(&format!(
            "- {:?}: {}{} bytes\n",
            member.name,
            if member.complete { "" } else { "≥" },
            member.value.len()
        ));
    }
    text.push_str(
        "Select a listed top-level key with symbols, or narrow the URL with server-side filters.",
    );
    let complete = text.len() <= URL_OUTPUT_BYTES - 1024;
    cap_text(
        &mut text,
        URL_OUTPUT_BYTES - 1024,
        "a filtered URL with fewer top-level keys",
    );
    Response::success(
        &req.id,
        serde_json::json!({"content": text, "kind": "json_keys", "complete": complete}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_spans_ignore_nested_delimiters_and_escaped_quotes() {
        let source = r#"{"a.b":{"nested":["},\"",1]},"unicode":"界","partial":[{"x":1}"#;
        let members = top_level_members(source);
        assert_eq!(members.len(), 3);
        assert_eq!(members[0].name, "a.b");
        assert_eq!(members[0].value, r#"{"nested":["},\"",1]}"#);
        assert!(members[0].complete);
        assert_eq!(members[1].value, "\"界\"");
        assert!(!members[2].complete);
    }

    #[test]
    fn text_cap_counts_bytes_and_preserves_utf8_and_footer() {
        let mut text = "界".repeat(50000);
        cap_text(&mut text, URL_OUTPUT_BYTES, "symbols");
        assert!(text.len() <= URL_OUTPUT_BYTES);
        assert!(text.ends_with("of 150000 bytes; narrow with symbols"));
    }

    fn assert_zoom_cap_preserves_metadata(mixed: bool) {
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let url = "https://example.com/large.md";
        let files = if mixed {
            vec!["/project/allowed.ts"]
        } else {
            vec![]
        };
        let mut targets = vec![serde_json::json!({"file": url, "symbols": "Large"})];
        if mixed {
            targets.insert(
                0,
                serde_json::json!({"file": "/project/allowed.ts", "symbols": "visible"}),
            );
        }
        let req = serde_json::from_value(serde_json::json!({
            "id": "capped-zoom", "command": "zoom",
            "targets": targets,
        }))
        .unwrap();
        let entries = serde_json::json!([{
            "targetLabel": url, "name": "Large",
            "response": {
                "success": true, "name": "Large", "kind": "heading",
                "content": "URL_CONTENT ".repeat(6000), "content_files": [],
            },
        }]);
        let mut data = serde_json::json!({"targets": entries, "content_files": files});
        if mixed {
            data["targets"].as_array_mut().unwrap().insert(
                0,
                serde_json::json!({
                    "targetLabel": "allowed.ts", "name": "visible",
                    "response": {
                        "success": true, "name": "visible", "kind": "function",
                        "content": "export function visible() {}", "content_files": files,
                    },
                }),
            );
        }
        let original_entries = data["targets"].clone();
        let response = cap_zoom_response(&req, &ctx, Response::success("capped-zoom", data));
        assert!(response.success);
        let text = response.data["text"]
            .as_str()
            .expect("capped rendered text");
        assert!(text.len() <= 50 * 1024, "{} bytes", text.len());
        assert!(text.contains("URL_CONTENT"));
        assert!(text.contains("truncated at"));
        assert_eq!(response.data["complete"], false);
        assert_eq!(response.data["content_files"], serde_json::json!(files));
        assert_eq!(response.data["targets"], original_entries);
    }

    #[test]
    fn zoom_url_cap_preserves_source_manifests_and_metadata() {
        assert_zoom_cap_preserves_metadata(false);
    }

    #[test]
    fn zoom_mixed_cap_preserves_source_manifests_and_metadata() {
        assert_zoom_cap_preserves_metadata(true);
    }

    #[test]
    fn outline_url_array_errors_redact_cached_document_paths() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("private-storage");
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.clone()),
                ..Default::default()
            },
        );
        let url = "https://example.com/docs.md";
        let req = serde_json::from_value(serde_json::json!({
            "id": "url-array-cache-redaction",
            "command": "outline",
            "files": ["local.rs", url]
        }))
        .unwrap();
        let cache_file = storage.join("url_cache").join("c7502988ff3e8388.md");
        let mut response = Response::success(
            "url-array-cache-redaction",
            serde_json::json!({
                "complete": false,
                "skipped_files": [{
                    "file": url,
                    "reason": format!("parse_error: syntax errors in {}", cache_file.display())
                }]
            }),
        );

        redact_url_cache_paths(&req, &ctx, &mut response);

        assert_eq!(response.data["skipped_files"][0]["file"], url);
        let reason = response.data["skipped_files"][0]["reason"]
            .as_str()
            .unwrap();
        assert!(reason.contains("the requested URL"), "{reason}");
        assert!(!reason.contains("url_cache"), "{reason}");
        assert!(!reason.contains("c7502988ff3e8388.md"), "{reason}");
    }

    #[test]
    fn url_responses_replace_cached_document_paths_with_the_requested_url() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("private-storage");
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.clone()),
                ..Default::default()
            },
        );
        let req = serde_json::from_value(serde_json::json!({
            "id": "url-cache-redaction",
            "command": "outline",
            "file": "https://example.com/docs.md"
        }))
        .unwrap();
        let cache_file = storage.join("url_cache").join("c7502988ff3e8388.md");
        let mut response = Response::error(
            "url-cache-redaction",
            "parse_error",
            format!("syntax errors in {}", cache_file.display()),
        );

        redact_url_cache_paths(&req, &ctx, &mut response);

        let message = response.data["message"].as_str().unwrap();
        assert!(message.contains("the requested URL"), "{message}");
        assert!(!message.contains("url_cache"), "{message}");
        assert!(!message.contains("c7502988ff3e8388.md"), "{message}");
    }

    /// With `storage.cache_dir` set, downloads live under the cache root on
    /// another path, and their paths must be redacted too.
    #[test]
    fn url_responses_redact_cached_document_paths_under_a_separate_cache_root() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("private-storage");
        let cache_parent = temp.path().join("cache-volume");
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.clone()),
                cache_dir: Some(cache_parent.clone()),
                ..Default::default()
            },
        );
        assert_ne!(ctx.cache_dir(), storage);
        let req = serde_json::from_value(serde_json::json!({
            "id": "url-cache-redaction-split",
            "command": "outline",
            "file": "https://example.com/docs.md"
        }))
        .unwrap();
        let cached = ctx
            .cache_dir()
            .join("url_cache")
            .join("c7502988ff3e8388.md");
        let legacy = storage.join("url_cache").join("0123456789abcdef.md");
        let mut response = Response::error(
            "url-cache-redaction-split",
            "parse_error",
            format!(
                "syntax errors in {} and {}",
                cached.display(),
                legacy.display()
            ),
        );

        redact_url_cache_paths(&req, &ctx, &mut response);

        let message = response.data["message"].as_str().unwrap();
        assert!(!message.contains("cache-volume"), "{message}");
        assert!(!message.contains("url_cache"), "{message}");
        assert!(!message.contains("c7502988ff3e8388.md"), "{message}");
        assert!(!message.contains("0123456789abcdef.md"), "{message}");
    }

    /// A placeholder fetch's private document lives beside the URL cache, and
    /// its path is redacted the same way.
    #[test]
    fn url_responses_redact_placeholder_fetch_document_paths() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("private-storage");
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.clone()),
                ..Default::default()
            },
        );
        let req = serde_json::from_value(serde_json::json!({
            "id": "secret-fetch-redaction",
            "command": "outline",
            "file": "https://example.com/docs.md?key={{ck-secret:test}}"
        }))
        .unwrap();
        let document = ctx
            .cache_dir()
            .join(crate::url_fetch::SECRET_FETCH_DIR)
            .join("0123456789abcdef")
            .join("document.md");
        let mut response = Response::error(
            "secret-fetch-redaction",
            "parse_error",
            format!("syntax errors in {}", document.display()),
        );

        redact_url_cache_paths(&req, &ctx, &mut response);

        let message = response.data["message"].as_str().unwrap();
        assert!(message.contains("the requested URL"), "{message}");
        assert!(
            !message.contains(crate::url_fetch::SECRET_FETCH_DIR),
            "{message}"
        );
        assert!(!message.contains("document.md"), "{message}");
    }
}
