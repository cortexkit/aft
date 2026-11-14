//! Dynamic parse gaps are recorded as typed AFT reasons.

use crate::callgraph_store::join::ManifestJoinError;
use crate::error::AftError;

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
