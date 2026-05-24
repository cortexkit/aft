#![allow(
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::double_ended_iterator_last,
    clippy::int_plus_one,
    clippy::large_enum_variant,
    clippy::len_without_is_empty,
    clippy::let_and_return,
    clippy::manual_contains,
    clippy::manual_pattern_char_comparison,
    clippy::manual_repeat_n,
    clippy::manual_strip,
    clippy::manual_unwrap_or_default,
    clippy::map_clone,
    clippy::iter_kv_map,
    clippy::needless_borrow,
    clippy::needless_borrows_for_generic_args,
    clippy::needless_range_loop,
    clippy::new_without_default,
    clippy::obfuscated_if_else,
    clippy::ptr_arg,
    clippy::question_mark,
    clippy::same_item_push,
    clippy::should_implement_trait,
    clippy::single_match,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::unnecessary_sort_by,
    clippy::unnecessary_cast,
    clippy::unnecessary_lazy_evaluations,
    clippy::unnecessary_map_or
)]

// ## Note on `.unwrap()` / `.expect()` usage
//
// The remaining `.unwrap()` and `.expect()` calls in `src/` are in:
// - **Tree-sitter query operations** (parser.rs, zoom.rs, extract.rs, inline.rs,
//   outline.rs): These operate on AFT's own compiled grammars and query patterns, which
//   are compile-time constants. Pattern captures and node kinds are guaranteed to exist.
// - **Checkpoint serialization** (checkpoint.rs): serde_json::to_value on known-good
//   HashMap<PathBuf, String> types cannot fail.
// - **lib.rs main loop**: JSON parsing of stdin lines — a malformed line is logged and
//   skipped, not unwrapped.
//
// All production command handlers that process user/agent input return Result or
// Response::error instead of panicking. Confirmed zero .unwrap()/.expect() in
// production error paths as of v0.6.3 audit.

pub mod ast_grep_hints;
pub mod ast_grep_lang;
pub mod backup;
pub mod bash_background;
pub mod bash_permissions;
pub mod bash_rewrite;
pub mod cache_freshness;
pub mod callgraph;
pub mod calls;
pub mod checkpoint;
pub mod commands;
pub mod compress;
pub mod config;
pub mod context;
pub mod db;
pub mod edit;
pub mod error;
pub mod extract;
pub mod format;
pub mod fs_lock;
pub mod fuzzy_match;
pub mod harness;
pub mod imports;
pub mod indent;
pub mod language;
pub mod log_ctx;
pub mod lsp;
pub mod lsp_hints;
pub mod migrate_storage;
pub mod parser;
pub mod protocol;
pub mod query_shape;
pub mod search_index;
pub mod semantic_index;
pub mod symbol_cache_disk;
pub mod symbols;
// Compiled on all platforms so cross-platform unit tests in
// `commands::bash::try_spawn_with_fallback` can exercise the retry
// decision logic without a real Windows runtime. The module itself only
// uses portable APIs; only its callers are Windows-gated.
pub mod windows_shell;

#[cfg(test)]
mod tests {
    use super::*;
    use config::Config;
    use error::AftError;
    use protocol::{RawRequest, Response};

    // --- Protocol serialization ---

    #[test]
    fn raw_request_deserializes_ping() {
        let json = r#"{"id":"1","command":"ping"}"#;
        let req: RawRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.id, "1");
        assert_eq!(req.command, "ping");
        assert!(req.lsp_hints.is_none());
    }

    #[test]
    fn raw_request_deserializes_echo_with_params() {
        let json = r#"{"id":"2","command":"echo","message":"hello"}"#;
        let req: RawRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.id, "2");
        assert_eq!(req.command, "echo");
        // "message" is captured in the flattened params
        assert_eq!(req.params["message"], "hello");
    }

    #[test]
    fn raw_request_preserves_unknown_fields() {
        let json = r#"{"id":"3","command":"ping","future_field":"abc","nested":{"x":1}}"#;
        let req: RawRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.params["future_field"], "abc");
        assert_eq!(req.params["nested"]["x"], 1);
    }

    #[test]
    fn raw_request_with_lsp_hints() {
        let json = r#"{"id":"4","command":"ping","lsp_hints":{"completions":["foo","bar"]}}"#;
        let req: RawRequest = serde_json::from_str(json).unwrap();
        assert!(req.lsp_hints.is_some());
        let hints = req.lsp_hints.unwrap();
        assert_eq!(hints["completions"][0], "foo");
    }

    #[test]
    fn response_success_round_trip() {
        let resp = Response::success("42", serde_json::json!({"command": "pong"}));
        let json_str = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(v["id"], "42");
        assert_eq!(v["success"], true);
        assert_eq!(v["command"], "pong");
    }

    #[test]
    fn response_error_round_trip() {
        let resp = Response::error("99", "unknown_command", "unknown command: foo");
        let json_str = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(v["id"], "99");
        assert_eq!(v["success"], false);
        assert_eq!(v["code"], "unknown_command");
        assert_eq!(v["message"], "unknown command: foo");
    }

    // --- Error formatting ---

    #[test]
    fn error_display_symbol_not_found() {
        let err = AftError::SymbolNotFound {
            name: "foo".into(),
            file: "bar.rs".into(),
        };
        assert_eq!(err.to_string(), "symbol 'foo' not found in bar.rs");
        assert_eq!(err.code(), "symbol_not_found");
    }

    #[test]
    fn error_display_ambiguous_symbol() {
        let err = AftError::AmbiguousSymbol {
            name: "Foo".into(),
            candidates: vec!["a.rs:10".into(), "b.rs:20".into()],
        };
        let s = err.to_string();
        assert!(s.contains("Foo"));
        assert!(s.contains("a.rs:10, b.rs:20"));
    }

    #[test]
    fn error_display_parse_error() {
        let err = AftError::ParseError {
            message: "unexpected token".into(),
        };
        assert_eq!(err.to_string(), "parse error: unexpected token");
    }

    #[test]
    fn error_display_file_not_found() {
        let err = AftError::FileNotFound {
            path: "/tmp/missing.rs".into(),
        };
        assert_eq!(err.to_string(), "file not found: /tmp/missing.rs");
    }

    #[test]
    fn error_display_invalid_request() {
        let err = AftError::InvalidRequest {
            message: "missing field".into(),
        };
        assert_eq!(err.to_string(), "invalid request: missing field");
    }

    #[test]
    fn error_display_checkpoint_not_found() {
        let err = AftError::CheckpointNotFound {
            name: "pre-refactor".into(),
        };
        assert_eq!(err.to_string(), "checkpoint not found: pre-refactor");
        assert_eq!(err.code(), "checkpoint_not_found");
    }

    #[test]
    fn error_display_no_undo_history() {
        let err = AftError::NoUndoHistory {
            path: "src/main.rs".into(),
        };
        assert_eq!(err.to_string(), "no undo history for: src/main.rs");
        assert_eq!(err.code(), "no_undo_history");
    }

    #[test]
    fn error_display_ambiguous_match() {
        let err = AftError::AmbiguousMatch {
            pattern: "TODO".into(),
            count: 5,
        };
        assert_eq!(
            err.to_string(),
            "pattern 'TODO' matches 5 occurrences, expected exactly 1"
        );
        assert_eq!(err.code(), "ambiguous_match");
    }

    #[test]
    fn error_display_project_too_large() {
        let err = AftError::ProjectTooLarge {
            count: 20001,
            max: 20000,
        };
        assert_eq!(
            err.to_string(),
            "project has 20001 source files, exceeding max_callgraph_files=20000. Call-graph operations (callers, trace_to, trace_data, impact) are disabled for this root. Open a specific subdirectory or raise max_callgraph_files in config."
        );
        assert_eq!(err.code(), "project_too_large");
    }

    #[test]
    fn error_to_json_has_code_and_message() {
        let err = AftError::FileNotFound { path: "/x".into() };
        let j = err.to_error_json();
        assert_eq!(j["code"], "file_not_found");
        assert!(j["message"].as_str().unwrap().contains("/x"));
    }

    // --- Config defaults ---

    #[test]
    fn config_default_values() {
        let cfg = Config::default();
        assert!(cfg.project_root.is_none());
        assert_eq!(cfg.validation_depth, 1);
        assert_eq!(cfg.checkpoint_ttl_hours, 24);
        assert_eq!(cfg.max_symbol_depth, 10);
        assert_eq!(cfg.formatter_timeout_secs, 10);
        assert_eq!(cfg.type_checker_timeout_secs, 30);
        assert_eq!(cfg.max_callgraph_files, 5_000);
    }
}
