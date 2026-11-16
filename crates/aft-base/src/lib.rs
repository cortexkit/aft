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
// Mirror the main crate's lint policy so moved code retains its lint baseline.

// Private fixture helpers can compile with test-support without their unit tests. Their local
// non-test dead-code allowances keep them private while preserving diagnostics in the unit harness.
// Keep one item body and its original production visibility while sharing it with higher-layer tests.
macro_rules! test_support_visibility {
    ($(#[$attribute:meta])* $visibility:vis $keyword:ident $($item:tt)*) => {
        #[cfg(any(test, feature = "test-support"))]
        $(#[$attribute])*
        pub $keyword $($item)*

        #[cfg(not(any(test, feature = "test-support")))]
        $(#[$attribute])*
        $visibility $keyword $($item)*
    };
}

pub mod agent_commands;
pub mod alert_records;
pub mod alloc_trace;
pub mod atomic_replace;
pub mod bash_env;
pub mod bounded_io;
pub mod build_state;
pub mod cache_freshness;
pub mod cache_location;
pub mod clock;
pub mod cold_build_limiter;
pub mod compress;
pub mod config;
pub mod config_fix;
pub mod config_resolve;
pub mod config_sources;
pub mod cross_volume;
pub mod developer_tools;
pub mod dir_identity;
pub mod disk_floor;
pub mod durability;
pub mod durable_dir;
pub mod effective_path;
pub mod engram_catalog;
#[doc(hidden)]
pub mod environment;
pub mod error;
#[cfg(any(test, feature = "test-support"))]
#[path = "../tests/helpers/executable_cache.rs"]
pub mod executable_cache;
pub mod executor;
pub mod feature_config;
pub mod file_identity;
pub mod file_kind;
pub mod fleet_status;
pub mod fs_lock;
pub mod gh_shim_paths;
pub mod git_head;
pub mod git_probe;
pub mod git_revision;
pub mod git_root_resolution;
pub mod github_address;
pub mod grammar_state;
pub mod harness;
pub mod harness_partition_guard;
pub mod host_tool_names;
pub mod inspect_category;
pub mod io_accounting;
pub mod jsonc;
pub mod jsonc_edit;
pub mod lang_id;
pub mod language;
pub mod large_read_counts;
#[doc(hidden)]
pub mod launch_fds;
pub mod launch_nonce;
pub mod registration;
pub mod list_envelope;
pub mod list_surfaces;
pub mod live_db_files;
pub mod lock_diagnostics;
pub mod log_ctx;
pub mod log_redact;
pub mod logging;
pub mod memory;

pub mod native_stat;
pub mod ndjson_text;
pub mod ort_lifecycle;
pub mod ort_pin;
pub mod os_metadata;
pub mod path_identity;
pub mod pattern_compile;
pub mod pending_response;
pub mod persistence_gate;
pub mod pinned_dir;
pub mod pinned_executable;
pub mod platform_tls;
pub mod policy_path;
#[cfg(unix)]
pub mod posix_spawn;
pub mod privacy_spawn;
#[doc(hidden)]
pub mod private_storage;
pub mod process_group;
pub mod process_identity;
pub mod process_io;
pub mod process_spawn;
pub mod production_storage;
pub mod project_content;
pub mod project_walk;
pub mod protocol;
pub mod pty_render;
pub mod query_shape;
pub mod read_policy;
pub mod secret;
pub mod scope_watch_events;
pub mod secret_proxy;
pub mod shared_timer;
pub mod shell_format;
pub mod stage_timer;
pub mod storage_path;
pub mod subc_config;
#[cfg(any(test, feature = "test-support"))]
pub mod subc_fake;
pub mod symbols;
pub mod system_temp;
pub mod task_temp_reads;
pub mod task_watch_lines;
pub mod task_watches;
#[cfg(any(test, feature = "test-support"))]
pub mod test_allocations;
#[cfg(any(test, feature = "test-support"))]
pub mod test_env;
#[cfg(any(test, feature = "test-support"))]
pub mod test_gate;
#[cfg(all(any(test, feature = "test-support"), windows))]
pub mod test_read_denial;
#[cfg(any(test, feature = "test-support"))]
pub mod test_storage;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod tool_gate;
pub mod tool_path;
pub mod tool_progress;
pub mod url_fetch;
pub mod view_storage_reserve;
pub mod volume;
pub mod walk_boundary;
#[cfg(any(test, feature = "test-support"))]
pub mod walk_profile;
pub mod watcher_markers;
pub mod windows_command;
pub mod windows_path;
pub mod worker_jail_canary;
pub mod write_counters;
pub mod write_intent;

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
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod search_hot_path_measurements;

#[cfg(any(test, feature = "test-support"))]
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
pub mod nested_worktree_tests;

#[cfg(all(any(test, feature = "test-support"), unix))]
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
pub mod storage_permissions_tests;

#[cfg(all(any(test, feature = "test-support"), target_os = "macos"))]
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
pub mod disk_write_hunt;
