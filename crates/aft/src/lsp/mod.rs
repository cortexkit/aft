pub mod child_registry;
pub mod client;
pub(crate) mod completed_rust_check;
pub mod diagnostics;
pub mod document;
pub mod environmental;
pub mod jsonrpc;
pub mod manager;
pub mod pending_changes;
pub mod position;
pub mod pull_params;
pub mod registry;
pub mod roots;
pub mod transport;
pub mod tsconfig_membership;
pub(crate) mod typescript_project;
mod writer;

/// LSP subsystem error type.
#[derive(Debug)]
pub enum LspError {
    Io(std::io::Error),
    Json(serde_json::Error),
    ServerNotReady(String),
    Timeout(String),
    ServerError { code: i32, message: String },
    NotFound(String),
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Json(err) => write!(f, "JSON error: {err}"),
            Self::ServerNotReady(message) => write!(f, "server not ready: {message}"),
            Self::Timeout(message) => write!(f, "timeout: {message}"),
            Self::ServerError { code, message } => {
                write!(f, "server error {code}: {message}")
            }
            Self::NotFound(message) => write!(f, "not found: {message}"),
        }
    }
}

impl std::error::Error for LspError {}

impl From<std::io::Error> for LspError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for LspError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}
