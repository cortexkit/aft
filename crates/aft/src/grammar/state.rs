use serde::Serialize;

use crate::error::AftError;

// Keep wording separate from the wire codes: callers branch on the codes, not prose.
pub const NOT_DOWNLOADED: &str =
    "The <language> grammar isn't downloaded yet; AFT downloads it on first use.";
pub const DOWNLOADING: &str = "The <language> grammar is downloading; retry in a few seconds.";
pub const NETWORK_RETRY: &str =
    "The <language> grammar download failed; AFT tries again after a short wait.";
pub const STORAGE_RETRY: &str =
    "AFT can't save the <language> grammar right now; it tries again after a short wait.";
pub const ATTEMPTS_EXHAUSTED: &str = "The <language> grammar couldn't be downloaded after five attempts; restarting AFT tries again.";
pub const VERIFICATION_FAILED: &str = "The downloaded <language> grammar didn't match its expected hash, so AFT didn't use it; restarting AFT tries again.";
pub const LOAD_FAILED: &str = "The <language> grammar failed to load; restarting AFT tries again.";
pub const UNSUPPORTED_PLATFORM: &str = "<language> files aren't supported on this platform yet.";
pub const SUPPORT_DISABLED: &str = "This AFT build has no downloaded-grammar support, so <language> files can't be parsed. Use a released AFT build, or rebuild with cargo install agent-file-tools --features wasm-grammars (needs CMake 3.12 or later).";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrammarState {
    SupportDisabled,
    NotDownloaded,
    Downloading,
    NetworkRetry,
    StorageRetry,
    AttemptsExhausted,
    VerificationFailed,
    LoadFailed,
    UnsupportedPlatform,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCause {
    Network,
    Storage,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GrammarUnavailable {
    pub language: String,
    pub state: GrammarState,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<FailureCause>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl GrammarUnavailable {
    pub(super) fn new(language: &str, state: GrammarState) -> Self {
        let message = match state {
            GrammarState::SupportDisabled => SUPPORT_DISABLED,
            GrammarState::NotDownloaded => NOT_DOWNLOADED,
            GrammarState::Downloading => DOWNLOADING,
            GrammarState::NetworkRetry => NETWORK_RETRY,
            GrammarState::StorageRetry => STORAGE_RETRY,
            GrammarState::AttemptsExhausted => ATTEMPTS_EXHAUSTED,
            GrammarState::VerificationFailed => VERIFICATION_FAILED,
            GrammarState::LoadFailed => LOAD_FAILED,
            GrammarState::UnsupportedPlatform => UNSUPPORTED_PLATFORM,
        };
        Self {
            language: language.into(),
            state,
            message: message.replace(
                "<language>",
                if state == GrammarState::SupportDisabled && language == "luau" {
                    "Luau"
                } else {
                    language
                },
            ),
            cause: None,
            retry_after_ms: None,
        }
    }
}

impl From<GrammarUnavailable> for AftError {
    fn from(value: GrammarUnavailable) -> Self {
        Self::GrammarUnavailable {
            language: value.language,
            state: value.state,
            message: value.message,
            cause: value.cause,
            retry_after_ms: value.retry_after_ms,
        }
    }
}
