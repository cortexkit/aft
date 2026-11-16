//! Dynamic parse gaps are recorded as typed AFT reasons.

use crate::callgraph_store::join::ManifestJoinError;
use crate::error::AftError;

const CALLGRAPH_OFF_MEANING: &str = "Luau files are skipped by meaning search because the call-graph index is off (indexes.callgraph: false).";

pub(crate) fn meaning_callgraph_off_reason() -> String {
    serde_json::json!({"code": "callgraph_off", "language": "luau", "state": "callgraph_off", "message": CALLGRAPH_OFF_MEANING}).to_string()
}

pub(crate) fn prepare(
    ctx: &crate::context::AppContext,
    path: &std::path::Path,
) -> Result<(), AftError> {
    if crate::parser::detect_language_any(path, true) == Some(crate::parser::LangId::Luau) {
        crate::grammar::configure(&ctx.config()).request()?;
    }
    Ok(())
}

pub(crate) fn error_response(id: &str, error: &AftError) -> crate::protocol::Response {
    let mut response = crate::protocol::Response::error(id, error.code(), error.to_string());
    response.data = error.to_error_json();
    if is_grammar_error(error) {
        response.data["complete"] = false.into();
    }
    response
}

pub(crate) fn error_response_for_path(
    id: &str,
    error: &AftError,
    path: &std::path::Path,
) -> crate::protocol::Response {
    let mut response = error_response(id, error);
    if is_grammar_error(error) {
        response.data["path"] = serde_json::json!(path);
    }
    response
}

pub(crate) fn is_grammar_error(error: &AftError) -> bool {
    matches!(
        error,
        AftError::GrammarUnavailable { .. } | AftError::GrammarParseTimeout { .. }
    )
}

/// Starting a scan's Luau extraction is the download trigger. The parser
/// itself remains stored-only, so unrelated parser users cannot download.
pub(super) fn request(language: &str) -> Result<(), ManifestJoinError> {
    if language == "luau" {
        crate::grammar::registry()
            .request()
            .map_err(ManifestJoinError::GrammarUnavailable)?;
    }
    Ok(())
}

/// Persist only stable codes. Messages, retry countdowns and timeout budgets
/// belong to the handling process and are reconstructed when a reply is made.
pub(super) fn reason(error: &ManifestJoinError) -> Option<String> {
    let value = match error {
        ManifestJoinError::GrammarUnavailable(error) => {
            serde_json::json!({"code": "grammar_unavailable", "state": error.state})
        }
        ManifestJoinError::GrammarParseTimeout { .. } => {
            serde_json::json!({"code": "grammar_parse_timeout", "state": "parse_timeout"})
        }
        _ => return None,
    };
    let reason = value.to_string();
    assert!(
        reason.len() <= 512,
        "host-owned grammar reason exceeds its bound"
    );
    Some(reason)
}

/// One non-blocking registry observation shared by every gap in a reply.
#[derive(Default)]
pub(crate) struct GapReply {
    unavailable: Option<crate::grammar::GrammarUnavailable>,
}

impl GapReply {
    pub(crate) fn capture() -> Self {
        #[cfg(feature = "wasm-grammars")]
        let unavailable = crate::grammar::registry().status_snapshot();
        #[cfg(not(feature = "wasm-grammars"))]
        let unavailable = None;
        Self { unavailable }
    }

    pub(crate) fn value(&self, stored: &str, path: &std::path::Path) -> Option<serde_json::Value> {
        let saved: serde_json::Value = serde_json::from_str(stored).ok()?;
        let mut value = match saved["code"].as_str()? {
            "grammar_parse_timeout" => AftError::GrammarParseTimeout {
                path: path.display().to_string(),
                language: "luau".into(),
                timeout_ms: crate::parser::luau_parse_deadline()
                    .as_millis()
                    .min(u64::MAX as u128) as u64,
            }
            .to_error_json(),
            "grammar_unavailable" => {
                let state = serde_json::from_value(saved["state"].clone()).ok()?;
                let error = self
                    .unavailable
                    .clone()
                    .unwrap_or_else(|| crate::grammar::GrammarUnavailable::new("luau", state));
                AftError::from(error).to_error_json()
            }
            "callgraph_off" => saved,
            _ => return None,
        };
        value["path"] = serde_json::json!(path);
        if value["code"] == "grammar_parse_timeout" {
            value["state"] = "parse_timeout".into();
        }
        Some(value)
    }
}

pub(super) fn extract(
    source: &[u8],
    language: &str,
) -> Result<aft_store::content::ContentRecord, ManifestJoinError> {
    request(language)?;
    #[cfg(test)]
    if extraction_for_test(source) == Some(TestExtraction::Failure) {
        return Err(ManifestJoinError::UnsupportedLanguage(language.into()));
    }
    let record = super::content::extract(source, language);
    #[cfg(test)]
    let record = record.map(|mut record| {
        if extraction_for_test(source) == Some(TestExtraction::NoOutline) {
            if let aft_store::content::RecordBody::Parsed { body, .. } = &mut record.data {
                body.outline = None;
            }
        }
        record
    });
    record
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq)]
pub(super) enum TestExtraction {
    Failure,
    NoOutline,
    MarkersOnly,
}

#[cfg(test)]
fn extraction_overrides(
) -> &'static std::sync::Mutex<std::collections::HashMap<aft_store::ids::ContentKey, TestExtraction>>
{
    static OVERRIDES: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<aft_store::ids::ContentKey, TestExtraction>>,
    > = std::sync::LazyLock::new(Default::default);
    &OVERRIDES
}

#[cfg(test)]
pub(super) fn extraction_for_test(source: &[u8]) -> Option<TestExtraction> {
    extraction_overrides()
        .lock()
        .unwrap()
        .get(&aft_store::ids::ContentKey::of(source))
        .copied()
}

#[cfg(all(
    test,
    feature = "wasm-grammars",
    feature = "test-hooks",
    not(all(target_os = "windows", target_arch = "aarch64"))
))]
pub(super) fn override_extraction_for_test(source: &[u8], outcome: TestExtraction) -> impl Drop {
    struct Guard(aft_store::ids::ContentKey);
    impl Drop for Guard {
        fn drop(&mut self) {
            extraction_overrides().lock().unwrap().remove(&self.0);
        }
    }
    let key = aft_store::ids::ContentKey::of(source);
    extraction_overrides().lock().unwrap().insert(key, outcome);
    Guard(key)
}

pub(crate) fn response(
    id: &str,
    reason: &str,
    path: &std::path::Path,
) -> Option<crate::protocol::Response> {
    let data = GapReply::capture().value(reason, path)?;
    let mut response =
        crate::protocol::Response::error(id, data["code"].as_str()?, data["message"].as_str()?);
    response.data = data;
    response.data["complete"] = false.into();
    response.data["path"] = serde_json::json!(path);
    Some(response)
}
