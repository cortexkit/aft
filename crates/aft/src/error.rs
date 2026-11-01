use std::fmt;

pub(crate) const FILE_CHANGED_SINCE_READ_MESSAGE: &str =
    "{path} changed on disk after this call read it; nothing was written. Read the file again and retry.";

/// Core error type for the aft binary.
///
/// Each variant maps to a structured error response with a `code` string
/// and a human-readable `message`.
#[derive(Debug)]
pub enum AftError {
    GrammarUnavailable {
        language: String,
        state: crate::grammar::GrammarState,
        message: String,
        cause: Option<crate::grammar::FailureCause>,
        retry_after_ms: Option<u64>,
    },
    SymbolNotFound {
        name: String,
        file: String,
    },
    AmbiguousSymbol {
        name: String,
        candidates: Vec<String>,
    },
    ParseError {
        message: String,
    },
    FileNotFound {
        path: String,
    },
    FileChangedSinceRead {
        path: String,
    },
    InvalidRequest {
        message: String,
    },
    CheckpointNotFound {
        name: String,
    },
    NoUndoHistory {
        path: String,
    },
    AmbiguousMatch {
        pattern: String,
        count: usize,
    },
    ScopeNotFound {
        scope: String,
        available: Vec<String>,
        file: String,
    },
    MemberNotFound {
        member: String,
        scope: String,
        file: String,
    },
    IoError {
        path: String,
        message: String,
    },
    /// A loose (whitespace-insensitive) match whose indentation differs from
    /// the file in a way that has no single safe re-indentation of the
    /// replacement. The edit is refused instead of guessing.
    AmbiguousIndentation {
        message: String,
    },
    DeniedFloor {
        path: String,
    },
}

impl AftError {
    /// Returns the error code string used in JSON error responses.
    pub fn code(&self) -> &'static str {
        match self {
            AftError::GrammarUnavailable { .. } => "grammar_unavailable",
            AftError::SymbolNotFound { .. } => "symbol_not_found",
            AftError::AmbiguousSymbol { .. } => "ambiguous_symbol",
            AftError::ParseError { .. } => "parse_error",
            AftError::FileNotFound { .. } => "file_not_found",
            AftError::FileChangedSinceRead { .. } => "file_changed_since_read",
            AftError::InvalidRequest { .. } => "invalid_request",
            AftError::CheckpointNotFound { .. } => "checkpoint_not_found",
            AftError::NoUndoHistory { .. } => "no_undo_history",
            AftError::AmbiguousMatch { .. } => "ambiguous_match",
            AftError::ScopeNotFound { .. } => "scope_not_found",
            AftError::MemberNotFound { .. } => "member_not_found",
            AftError::IoError { .. } => "io_error",
            AftError::AmbiguousIndentation { .. } => "ambiguous_indentation",
            AftError::DeniedFloor { .. } => "denied_floor",
        }
    }

    /// Produces a `serde_json::Value` suitable for the error portion of a response.
    ///
    /// Shape: `{ "code": "...", "message": "..." }`
    pub fn to_error_json(&self) -> serde_json::Value {
        if let Self::GrammarUnavailable {
            language,
            state,
            message,
            cause,
            retry_after_ms,
        } = self
        {
            let mut value = serde_json::json!({
                "code": self.code(), "language": language, "state": state, "message": message,
            });
            if let Some(cause) = cause {
                value["cause"] = serde_json::json!(cause);
            }
            if let Some(retry_after_ms) = retry_after_ms {
                value["retry_after_ms"] = serde_json::json!(retry_after_ms);
            }
            return value;
        }
        serde_json::json!({
            "code": self.code(),
            "message": self.to_string(),
        })
    }
}

impl fmt::Display for AftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AftError::GrammarUnavailable { message, .. } => f.write_str(message),
            AftError::DeniedFloor { path } => {
                f.write_str(&crate::access_policy::aft_storage_denial_message(path))
            }
            AftError::SymbolNotFound { name, file } => {
                write!(f, "symbol '{}' not found in {}", name, file)
            }
            AftError::AmbiguousSymbol { name, candidates } => {
                write!(
                    f,
                    "symbol '{}' is ambiguous, candidates: [{}]",
                    name,
                    candidates.join(", ")
                )
            }
            AftError::ParseError { message } => {
                write!(f, "parse error: {}", message)
            }
            AftError::FileNotFound { path } => {
                write!(f, "file not found: {}", path)
            }
            AftError::FileChangedSinceRead { path } => {
                f.write_str(&FILE_CHANGED_SINCE_READ_MESSAGE.replace("{path}", path))
            }
            AftError::InvalidRequest { message } => {
                write!(f, "invalid request: {}", message)
            }
            AftError::CheckpointNotFound { name } => {
                write!(f, "checkpoint not found: {}", name)
            }
            AftError::NoUndoHistory { path } => {
                write!(f, "no undo history for: {}", path)
            }
            AftError::AmbiguousMatch { pattern, count } => {
                write!(
                    f,
                    "pattern '{}' matches {} occurrences, expected exactly 1",
                    pattern, count
                )
            }
            AftError::ScopeNotFound {
                scope,
                available,
                file,
            } => {
                if available.is_empty() {
                    write!(
                        f,
                        "scope '{}' not found in {} (no scopes available)",
                        scope, file
                    )
                } else {
                    write!(
                        f,
                        "scope '{}' not found in {}, available: [{}]",
                        scope,
                        file,
                        available.join(", ")
                    )
                }
            }
            AftError::MemberNotFound {
                member,
                scope,
                file,
            } => {
                write!(
                    f,
                    "member '{}' not found in scope '{}' in {}",
                    member, scope, file
                )
            }
            AftError::IoError { path, message } => {
                write!(f, "I/O error on {}: {}", path, message)
            }
            AftError::AmbiguousIndentation { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for AftError {}
