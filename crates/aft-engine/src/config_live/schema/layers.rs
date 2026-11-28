use serde_json::{json, Map, Value};

pub(super) const NEXT_SESSION: &str = "Applies to sessions that start after the change.";

pub(crate) fn user_only(path: &str) -> bool {
    const SECTIONS: &[&str] = &[
        "subc",
        "bridge",
        "github",
        "gh_shim",
        "auto_update",
        "index.roots",
        "shell.env_provider",
        "lsp.servers",
        "lsp.versions",
        "lsp.disabled",
        "bash.runon_env",
        "storage",
        "durability",
        "memory",
        "sandbox.read_deny",
        "sandbox.write_allow",
        "disabled_tools",
    ];
    SECTIONS.iter().any(|section| {
        path == *section
            || path
                .strip_prefix(section)
                .is_some_and(|suffix| suffix.starts_with('.'))
    }) || matches!(
        path,
        "semantic.backend"
            | "semantic.base_url"
            | "semantic.api_key_env"
            | "semantic.query_timeout_ms"
            | "search.rerank.model"
            | "search.rerank.endpoint"
            | "search.rerank.api_key_env"
            | "search.rerank.top_n"
            | "search.rerank.timeout_ms"
            | "lsp.auto_install"
            | "lsp.grace_days"
            | "bash.linux_scope"
            | "bash.linux_scope_max_runtime_secs"
            | "remote_exec.default_demand"
            | "formatter_timeout_secs"
            | "type_checker_timeout_secs"
            | "backup.enabled"
            | "backup.max_depth"
            | "restrict_to_project_root"
            | "url_fetch_allow_private"
    )
}

pub(super) fn set(node: &mut Map<String, Value>, path: &str) {
    node.insert(
        "x-ck-layers".into(),
        if user_only(path) {
            json!(["user"])
        } else {
            json!(["user", "workspace", "project"])
        },
    );
    let (bound, ordering) = match path {
        "sandbox.profile.worker" | "sandbox.profile.head" => {
            ("raise_only", Some(json!(["off", "relaxed", "strict"])))
        }
        "remote_exec.enabled" | "indexes.trigram" | "indexes.semantic" | "indexes.callgraph" => {
            ("lower_only", Some(json!([false, true])))
        }
        "lsp.max_servers"
        | "lsp.worker_rust_analyzers"
        | "lsp.idle_minutes"
        | "bash.memory_limit_mib" => ("lower_only", None),
        "lsp.idle_shutdown" | "bash.disclaim_privacy" | "pi.external_access_ask" => {
            ("raise_only", Some(json!([false, true])))
        }
        "inspect.diagnostics_timeout_ms" => ("raise_only", None),
        path if path.starts_with("inspect.categories.") => {
            ("lower_only", Some(json!([false, true])))
        }
        _ => return,
    };
    node.insert("x-ck-bound".into(), json!(bound));
    if let Some(ordering) = ordering {
        node.insert("x-ck-ordering".into(), ordering);
    }
}

pub(super) fn next_session(path: &str) -> bool {
    matches!(
        path,
        "edit_mode"
            | "bash.background"
            | "bash.detach_on_user_message"
            | "bash.powershell_tool"
            | "semantic.model"
            | "semantic.timeout_ms"
            | "semantic.max_batch_size"
            | "semantic.max_input_tokens"
            | "semantic.max_files"
    )
}
