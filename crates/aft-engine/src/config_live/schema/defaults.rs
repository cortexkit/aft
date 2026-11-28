use crate::config_resolve::resolve_config;
use serde_json::{json, Value};

/// Project runtime defaults into the file's spelling. Null means there is no
/// default of the declared type. The compatibility keys that only the plugins
/// consume use the plugins' defaults.
pub(super) fn values() -> Value {
    let config = resolve_config(&[]).config;
    let mut semantic = json!(config.semantic);
    // The Rust resolver supplies the local model when omitted. Synapse needs
    // an explicit choice, so leave the model unset in the effective view.
    semantic.as_object_mut().unwrap().remove("model");
    let mut bash = json!(config.bash);
    for (key, value) in [
        ("rewrite", json!(config.experimental_bash_rewrite)),
        ("compress", json!(config.experimental_bash_compress)),
        ("background", json!(config.experimental_bash_background)),
        ("subagent_background", json!(true)),
        ("env", json!(config.bash.env)),
        ("runon_env", json!(config.bash.runon_env)),
        (
            "long_running_reminder_enabled",
            json!(config.bash_long_running_reminder_enabled),
        ),
        (
            "long_running_reminder_interval_ms",
            json!(config.bash_long_running_reminder_interval_ms),
        ),
        (
            "foreground_wait_window_ms",
            json!(config.foreground_wait_window_ms),
        ),
    ] {
        bash[key] = value;
    }
    json!({
        "durability": config.durability,
        "storage": {"cache_dir": {"kind": "default"}},
        "edit_mode": if config.hashline_enabled {"hashline"} else {"default"},
        "format_on_edit": config.format_on_edit,
        "revision_reads": config.revision_reads,
        "formatter_timeout_secs": config.formatter_timeout_secs,
        "type_checker_timeout_secs": config.type_checker_timeout_secs,
        "validate_on_edit": config.validate_on_edit,
        "formatter": config.formatter,
        "checker": config.checker,
        "configure_warnings_delivery": "toast",
        "disabled_tools": config.disabled_tools,
        "restrict_to_project_root": config.restrict_to_project_root,
        "indexes": config.indexes,
        "index": config.index,
        "inspect": config.inspect,
        "idle": config.idle,
        "backup": config.backup,
        "github": config.github,
        "gh_shim": config.gh_shim,
        "git": config.git,
        "pi": {"tool_presentation": "top_level", "external_access_ask": true},
        "sandbox": config.sandbox,
        "bash": bash,
        "shell": config.shell,
        "experimental": {"lsp_ty": config.experimental_lsp_ty},
        "lsp": {
            "idle_minutes": config.lsp_idle_minutes,
            "idle_shutdown": config.lsp_idle_shutdown,
            "max_servers": config.lsp_max_servers,
            "worker_rust_analyzers": config.lsp_worker_rust_analyzers,
            "servers": {},
            "disabled": config.disabled_lsp,
            "python": "auto",
            "diagnostics_on_edit": config.diagnostics_on_edit,
            "auto_install": true,
            "grace_days": 7,
            "versions": {},
        },
        "url_fetch_allow_private": config.url_fetch_allow_private,
        "semantic": semantic,
        "search": config.search,
        "auto_update": true,
        "bridge": {"request_timeout_ms": 30_000},
        "subc": {"client_reaper": false},
        "remote_exec": config.remote_exec,
    })
}
