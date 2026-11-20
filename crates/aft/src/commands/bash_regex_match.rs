use crate::protocol::{RawRequest, Response};
use crate::task_watches::WatchPattern;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
struct BashRegexMatchParams {
    pattern: String,
    text: String,
    /// Byte offset in `text` where the search starts. Text before it is
    /// context only: `^`, `\b` and `\B` at the start position see the real
    /// preceding character instead of treating the start as the start of
    /// input. A plugin watch passes the character just before its trimmed
    /// scan window here. Omitted means 0.
    #[serde(default)]
    start: Option<usize>,
}

#[derive(Debug, PartialEq, Eq)]
struct BashRegexMatchResult {
    matched: bool,
    match_text: Option<String>,
    match_offset: Option<u64>,
    match_index_chars: Option<usize>,
}

fn regex_match(
    pattern: &str,
    text: &str,
    start: usize,
) -> Result<BashRegexMatchResult, regex::Error> {
    let pattern = WatchPattern::regex(pattern)?;
    let WatchPattern::Regex(regex) = pattern else {
        unreachable!("WatchPattern::regex always returns the Regex variant");
    };

    let Some(found) = regex.find_at(text, start) else {
        return Ok(BashRegexMatchResult {
            matched: false,
            match_text: None,
            match_offset: None,
            match_index_chars: None,
        });
    };

    let start = found.start();
    Ok(BashRegexMatchResult {
        matched: true,
        match_text: Some(found.as_str().to_string()),
        match_offset: Some(start as u64),
        match_index_chars: Some(text[..start].chars().count()),
    })
}

pub fn handle(req: &RawRequest) -> Response {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    let params = match serde_json::from_value::<BashRegexMatchParams>(raw_params) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash_regex_match: invalid params: {e}"),
            );
        }
    };

    let start = params.start.unwrap_or(0);
    if start > params.text.len() || !params.text.is_char_boundary(start) {
        return Response::error(
            &req.id,
            "invalid_request",
            format!(
                "bash_regex_match: start {start} is not a character boundary within the {}-byte text",
                params.text.len()
            ),
        );
    }
    match regex_match(&params.pattern, &params.text, start) {
        Ok(result) => {
            let mut data = if result.matched {
                json!({
                    "matched": true,
                    "match_text": result.match_text,
                    "match_offset": result.match_offset,
                    "match_index_chars": result.match_index_chars,
                })
            } else {
                json!({ "matched": false })
            };
            // Echo an explicit start, so a caller can tell this engine honoured
            // it from an older one that ignores the unknown field.
            if params.start.is_some() {
                data["start"] = json!(start);
            }
            Response::success(&req.id, data)
        }
        Err(e) => Response::error(
            &req.id,
            "invalid_regex",
            format!("bash_regex_match: invalid regex: {e}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn catastrophic_backtracking_pattern_returns_quickly_without_match() {
        let text = "a".repeat(65_536);
        let started = Instant::now();

        let result = regex_match("(a+)+b", &text, 0).expect("regex should compile");

        assert!(!result.matched);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "Rust regex matching should be linear for catastrophic JS patterns"
        );
    }

    #[test]
    fn normal_match_reports_byte_offset_and_char_index() {
        let result = regex_match("ready", "αβ ready", 0).expect("regex should compile");

        assert_eq!(
            result,
            BashRegexMatchResult {
                matched: true,
                match_text: Some("ready".to_string()),
                match_offset: Some(5),
                match_index_chars: Some(3),
            }
        );
    }

    #[test]
    fn regex_uses_multiline_anchors_like_async_watches() {
        let result = regex_match("^foo$", "bar\nfoo\nbaz", 0).expect("regex should compile");

        assert_eq!(
            result,
            BashRegexMatchResult {
                matched: true,
                match_text: Some("foo".to_string()),
                match_offset: Some(4),
                match_index_chars: Some(4),
            }
        );
    }

    fn request(params: serde_json::Value) -> RawRequest {
        serde_json::from_value(json!({
            "id": "regex-start",
            "command": "bash_regex_match",
            "params": params,
        }))
        .unwrap()
    }

    fn wire(params: serde_json::Value) -> serde_json::Value {
        serde_json::to_value(handle(&request(params))).unwrap()
    }

    #[test]
    fn approved_start_gives_line_anchor_the_real_preceding_character() {
        // "x" before the start: ^ cannot match there.
        let reply = wire(json!({ "pattern": "^READY", "text": "xREADY", "start": 1 }));
        assert_eq!(reply["matched"], false, "{reply}");
        assert_eq!(reply["start"], 1);
        // A newline before the start: ^ matches at the start.
        let reply = wire(json!({ "pattern": "^READY", "text": "\nREADY", "start": 1 }));
        assert_eq!(reply["matched"], true, "{reply}");
        assert_eq!(reply["match_offset"], 1);
    }

    #[test]
    fn approved_start_gives_word_boundaries_the_real_preceding_character() {
        let reply = wire(json!({ "pattern": r"\bREADY", "text": "xREADY", "start": 1 }));
        assert_eq!(reply["matched"], false, "{reply}");
        let reply = wire(json!({ "pattern": r"\bREADY", "text": " READY", "start": 1 }));
        assert_eq!(reply["matched"], true, "{reply}");
        let reply = wire(json!({ "pattern": r"\BREADY", "text": "xREADY", "start": 1 }));
        assert_eq!(reply["matched"], true, "{reply}");
        assert_eq!(reply["match_offset"], 1);
        let reply = wire(json!({ "pattern": r"\BREADY", "text": " READY", "start": 1 }));
        assert_eq!(reply["matched"], false, "{reply}");
    }

    #[test]
    fn approved_start_skips_matches_before_it() {
        let reply = wire(json!({ "pattern": "ab", "text": "ab ab", "start": 1 }));
        assert_eq!(reply["match_offset"], 3, "{reply}");
    }

    #[test]
    fn approved_omitted_start_behaves_as_before_and_is_not_echoed() {
        let reply = wire(json!({ "pattern": "^READY", "text": "READY" }));
        assert_eq!(reply["matched"], true);
        assert_eq!(reply["match_offset"], 0);
        assert!(reply.get("start").is_none(), "{reply}");
    }

    #[test]
    fn approved_start_off_a_char_boundary_or_past_the_text_is_refused() {
        for start in [1, 9] {
            let reply = wire(json!({ "pattern": "b", "text": "\u{e9}b", "start": start }));
            assert_eq!(reply["success"], false, "{reply}");
            assert_eq!(reply["code"], "invalid_request", "{reply}");
        }
    }
}
