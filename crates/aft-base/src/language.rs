use std::path::Path;

use crate::error::AftError;
pub use crate::symbols::{Range, Symbol, SymbolMatch};

/// An explicitly declared heading anchor used as a URL fragment, with its source location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadingAnchor {
    pub start_line: u32,
    pub start_col: u32,
    pub id: String,
}

/// Trait for language-specific symbol resolution.
///
/// S02 implements this with tree-sitter parsing via `TreeSitterProvider`.
pub trait LanguageProvider: Send + Sync {
    /// Resolve a symbol by name within a file. Returns all matches.
    fn resolve_symbol(&self, file: &Path, name: &str) -> Result<Vec<SymbolMatch>, AftError>;

    /// List all top-level symbols in a file.
    fn list_symbols(&self, file: &Path) -> Result<Vec<Symbol>, AftError>;

    /// Return explicitly declared heading anchors and their source locations.
    fn heading_anchors(&self, _file: &Path) -> Result<Vec<HeadingAnchor>, AftError> {
        Ok(Vec::new())
    }

    /// Downcast to concrete type for provider-specific operations.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Placeholder provider that rejects all calls.
///
/// Retained for tests and fallback. Production code uses `TreeSitterProvider`.
pub struct StubProvider;

impl LanguageProvider for StubProvider {
    fn resolve_symbol(&self, _file: &Path, _name: &str) -> Result<Vec<SymbolMatch>, AftError> {
        Err(AftError::InvalidRequest {
            message: "no language provider configured".to_string(),
        })
    }

    fn list_symbols(&self, _file: &Path) -> Result<Vec<Symbol>, AftError> {
        Err(AftError::InvalidRequest {
            message: "no language provider configured".to_string(),
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
