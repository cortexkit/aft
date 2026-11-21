//! Per-diagnostic classification for environment/setup failures vs real code issues.
//!
//! Environmental diagnostics (missing TypeScript install, JSON schema fetch
//! failures for editor tooling, etc.) may appear in the warm LSP set but must not
//! inflate the agent status bar `E`/`W` counts or `aft_inspect` diagnostics
//! summary totals — only per-diagnostic, not per-file.

use super::diagnostics::StoredDiagnostic;

/// typescript-language-server 5.x: no usable TypeScript SDK was found.
pub(crate) const TS_LS5_NO_INSTALLATION: &str = "Could not find a valid TypeScript installation";
/// typescript-language-server 6.x: the TypeScript it found ships no
/// `lib/tsserver.js` (TypeScript 7 and later are the native compiler and
/// ship none), and nothing else was usable.
pub(crate) const TS_LS6_NO_TSSERVER: &str = "provides no tsserver.js";
/// AFT's own wording when the project's TypeScript is the native compiler.
pub(crate) const TS_NATIVE_NO_TSSERVER: &str =
    "which has no tsserver; typescript-language-server can't serve it";

/// True when this diagnostic reflects the tooling/environment, not project source.
pub fn is_environmental_diagnostic(diagnostic: &StoredDiagnostic) -> bool {
    let message = diagnostic.message.as_str();
    let code = diagnostic.code.as_deref().unwrap_or("");
    is_environmental_message(message, code)
}

fn is_environmental_message(message: &str, code: &str) -> bool {
    let lower = message.to_ascii_lowercase();

    // TypeScript language service / tsserver environment failures, in both
    // typescript-language-server wordings and AFT's native-compiler rewrite.
    if [
        TS_LS5_NO_INSTALLATION,
        TS_LS6_NO_TSSERVER,
        TS_NATIVE_NO_TSSERVER,
    ]
    .iter()
    .any(|phrase| lower.contains(&phrase.to_ascii_lowercase()))
    {
        return true;
    }

    // JSON / $schema fetch failures (vscode-json-languageservice and peers).
    if message_contains_schema_fetch_failure(&lower) {
        return true;
    }

    // Known TS codes for config / project setup (not per-line source defects).
    matches!(code, "18003" | "TS18003" | "6053" | "TS6053")
        || lower.contains("no inputs were found in config file")
}

fn message_contains_schema_fetch_failure(lower: &str) -> bool {
    const FETCH_VERBS: &[&str] = &[
        "failed to fetch",
        "failed to load",
        "failed to download",
        "failed to resolve",
        "failed to read",
        "unable to load",
        "unable to fetch",
        "unable to resolve",
        "could not load",
        "could not fetch",
        "could not resolve",
        "could not download",
        "error loading",
        "error fetching",
        "error resolving",
    ];
    // Verb + "schema" alone is too loose: real code diagnostics can contain
    // both (Rust "failed to resolve: use of undeclared crate or module
    // `schema`", ESLint import-resolution on a ./schema module). Genuine
    // language-server schema-fetch failures always reference the remote
    // schema by URL — require one. Hiding a real error is strictly worse
    // than counting an environmental one, so favor false negatives.
    let references_remote_schema =
        lower.contains("schema") && (lower.contains("http://") || lower.contains("https://"));
    references_remote_schema && FETCH_VERBS.iter().any(|verb| lower.contains(verb))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::is_environmental_diagnostic;
    use crate::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};

    fn stored(message: &str, code: Option<&str>, source: Option<&str>) -> StoredDiagnostic {
        StoredDiagnostic {
            file: PathBuf::from("/repo/src/app.ts"),
            line: 1,
            column: 1,
            end_line: 1,
            end_column: 2,
            severity: DiagnosticSeverity::Error,
            message: message.into(),
            code: code.map(str::to_string),
            source: source.map(str::to_string),
        }
    }

    #[test]
    fn typescript_installation_is_environmental() {
        assert!(is_environmental_diagnostic(&stored(
            "Could not find a valid TypeScript installation. Try `npm i typescript`.",
            None,
            Some("typescript"),
        )));
    }

    #[test]
    fn typescript_language_server_6_missing_tsserver_is_environmental() {
        assert!(is_environmental_diagnostic(&stored(
            "Request initialize failed with message: The TypeScript of the workspace (TypeScript 7.0.2 at \"/repo/node_modules/typescript/lib\") provides no tsserver.js. No other valid TypeScript installation was found. Exiting.",
            None,
            Some("typescript"),
        )));
    }

    #[test]
    fn typescript_native_compiler_rewrite_is_environmental() {
        assert!(is_environmental_diagnostic(&stored(
            "TypeScript unavailable: this project uses TypeScript 7.0.2 (native compiler) at /repo/node_modules/typescript, which has no tsserver; typescript-language-server can't serve it.",
            None,
            Some("typescript"),
        )));
    }

    #[test]
    fn real_error_mentioning_tsserver_is_not_environmental() {
        assert!(!is_environmental_diagnostic(&stored(
            "Cannot find module './tsserver' or its corresponding type declarations.",
            Some("TS2307"),
            Some("typescript"),
        )));
    }

    #[test]
    fn schema_fetch_failure_is_environmental() {
        assert!(is_environmental_diagnostic(&stored(
            "Failed to fetch schema from https://example.com/schema.json: getaddrinfo ENOTFOUND",
            None,
            Some("json"),
        )));
    }

    #[test]
    fn real_syntax_error_is_not_environmental() {
        assert!(!is_environmental_diagnostic(&stored(
            "Type 'string' is not assignable to type 'number'.",
            Some("TS2322"),
            Some("typescript"),
        )));
    }

    #[test]
    fn rust_crate_named_schema_is_not_environmental() {
        // Real compiler error mentioning a crate/module named `schema` must
        // keep counting as an error (oracle finding: verb+keyword was too loose).
        assert!(!is_environmental_diagnostic(&stored(
            "failed to resolve: use of undeclared crate or module `schema`",
            Some("E0433"),
            Some("rust-analyzer"),
        )));
    }

    #[test]
    fn local_schema_module_import_error_is_not_environmental() {
        assert!(!is_environmental_diagnostic(&stored(
            "Unable to resolve path to module './schema'.",
            Some("import/no-unresolved"),
            Some("eslint"),
        )));
    }

    #[test]
    fn global_types_typo_is_not_environmental() {
        assert!(!is_environmental_diagnostic(&stored(
            "Cannot find global type 'NotARealPackage'.",
            Some("TS2688"),
            Some("typescript"),
        )));
    }

    #[test]
    fn mixed_file_classifier_is_per_diagnostic() {
        let syntax = stored(
            "Cannot find name 'foo'.",
            Some("TS2304"),
            Some("typescript"),
        );
        let schema = stored(
            "Failed to load schema from https://cdn.example/pkg/schema.json",
            None,
            Some("json"),
        );
        assert!(!is_environmental_diagnostic(&syntax));
        assert!(is_environmental_diagnostic(&schema));
    }
}
