use std::path::Path;

use crate::error::AftError;
pub use crate::symbols::{Range, Symbol, SymbolMatch};

/// Trait for language-specific symbol resolution.
///
/// S02 implements this with tree-sitter parsing via `TreeSitterProvider`.
pub trait LanguageProvider {
    /// Resolve a symbol by name within a file. Returns all matches.
    fn resolve_symbol(&self, file: &Path, name: &str) -> Result<Vec<SymbolMatch>, AftError>;

    /// List all top-level symbols in a file.
    fn list_symbols(&self, file: &Path) -> Result<Vec<Symbol>, AftError>;

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
