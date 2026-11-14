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
    response
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

/// Persist only typed, bounded host-owned fields. Timeout paths are supplied
/// by the checkout when displayed, not embedded in every reason string.
pub(super) fn reason(error: &ManifestJoinError) -> Option<String> {
    let value = match error {
        ManifestJoinError::GrammarUnavailable(error) => {
            AftError::from(error.clone()).to_error_json()
        }
        ManifestJoinError::GrammarParseTimeout {
            language,
            timeout_ms,
            ..
        } => {
            serde_json::json!({"code": "grammar_parse_timeout", "language": language,
                "state": "parse_timeout", "timeout_ms": timeout_ms})
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

pub(super) fn message(reason: &str, path: &std::path::Path) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(reason).ok()?;
    if value["code"] == "grammar_parse_timeout" {
        return Some(
            AftError::GrammarParseTimeout {
                path: path.display().to_string(),
                language: "luau".into(),
                timeout_ms: value["timeout_ms"].as_u64()?,
            }
            .to_string(),
        );
    }
    value["message"].as_str().map(str::to_owned)
}

/// The stored reason records the scan's state. For `grammar_unavailable`,
/// check the grammar registry again so replies do not keep reporting an
/// in-flight download after it has failed.
pub(super) fn current_reason(stored: &str) -> String {
    if cfg!(feature = "wasm-grammars")
        && serde_json::from_str::<serde_json::Value>(stored)
            .is_ok_and(|value| value["code"] == "grammar_unavailable")
    {
        if let Err(error) = crate::grammar::registry().stored() {
            return reason(&ManifestJoinError::GrammarUnavailable(error))
                .expect("typed grammar reason");
        }
    }
    stored.to_string()
}

pub(crate) fn gap_value(stored: &str, path: &std::path::Path) -> Option<serde_json::Value> {
    let reason = current_reason(stored);
    let mut value: serde_json::Value = serde_json::from_str(&reason).ok()?;
    value["message"] = message(&reason, path)?.into();
    value["path"] = serde_json::json!(path);
    Some(value)
}

pub(super) fn extract(
    source: &[u8],
    language: &str,
) -> Result<aft_store::content::ContentRecord, ManifestJoinError> {
    request(language)?;
    super::content::extract(source, language)
}

pub(crate) fn response(
    id: &str,
    reason: &str,
    path: &std::path::Path,
) -> Option<crate::protocol::Response> {
    let current = current_reason(reason);
    let reason = current.as_str();
    let mut data: serde_json::Value = serde_json::from_str(reason).ok()?;
    data["message"] = message(reason, path)?.into();
    if data["code"] == "grammar_parse_timeout" {
        data["path"] = path.display().to_string().into();
    }
    let mut response =
        crate::protocol::Response::error(id, data["code"].as_str()?, data["message"].as_str()?);
    response.data = data;
    Some(response)
}
