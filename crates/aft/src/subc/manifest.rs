//! Manifest, lane classification, and control-surface helpers exposed over subc.

use super::{
    json, Bindings, Concurrency, ConsumerRole, ExecutionMode, Flags, IdentityBinding,
    IdentityScope, Lane, LazyLock, ManagementOperation, ManagementOperationKind, ModuleManifest,
    Priority, ProviderRole, StorageBinding, StorageKind, StorageScope, Tool, TrustTier, Value,
    MODULE_CONTROL_OP_HEALTH_CHECK, PROTOCOL_VERSION,
};

pub(super) fn is_bash_family_tool(name: &str) -> bool {
    name == "bash" || name == "powershell" || name.starts_with("bash_")
}

pub(super) fn is_subc_agent_core_tool(name: &str) -> bool {
    matches!(
        name,
        "status"
            | "bash"
            | "powershell"
            | "read"
            | "write"
            | "edit"
            | "apply_patch"
            | "grep"
            | "glob"
            | "search"
            | "outline"
            | "zoom"
            | "inspect"
            | "callgraph"
            | "conflicts"
            | "ast_search"
            | "ast_replace"
            | "delete"
            | "move"
            | "import"
            | "safety"
            | "bash_status"
            | "bash_kill"
            | "bash_write"
    )
}

/// Internal plumbing commands the harness consumer (NOT the agent) invokes over
/// a bound route. These are NOT agent-facing tools — they carry no agent surface
/// and never reach the model — so they're not in the manifest /
/// `is_subc_agent_core_tool`, but the plugin must reach dispatch with them over
/// subc for background-bash delivery and safety undo/restore to work.
///
/// This is a DELIBERATELY TIGHT allowlist, kept separate from the agent
/// core-tool gate so it cannot widen the fail-closed backstop in
/// `handle_tool_call`. Every entry is session-scoped (the bind session is
/// reinjected by `run_tool_call`, overriding any body `session_id`) and carries
/// NO config/trust surface, so admitting them does not reopen the
/// `configure`-bypass hole the gate exists to close. The untrusted-bind bash
/// denial fires BEFORE this allowlist (`is_bash_family_tool` matches every
/// `bash_*` name), so untrusted binds still cannot observe bash state:
/// - `bash_abort_inflight`: abort-only per-session cancellation of foreground
///   bash calls that are still wait-registered; explicit background and PTY
///   tasks are not registered and are therefore untouched.
/// - `bash_status`: read-only per-session task snapshot; required so a
///   respawned module can report rehydrated detached tasks by task id. It is
///   also an agent tool (see `is_subc_agent_core_tool`); it stays listed here
///   so the plugins' own status polling keeps being treated as plumbing (for
///   example, it is not counted by the repeated-call breaker).
/// - `bash_drain_completions` / `bash_ack_completions`: per-session completion
///   registry plumbing for the bg_events wake lane (drain = PureRead,
///   ack = Mutating in `command_lane`).
/// - `undo_preview` / `checkpoint_paths`: read-only permission-preview reads
///   over the session's own backup/checkpoint state — the plugin safety tool
///   calls them BEFORE `aft_safety undo`/`restore` to know which paths to ask
///   permission for. Without them, safety undo/restore fails over subc.
/// - `bash_kill` / `bash_write` / `bash_notify` / `bash_unnotify` /
///   `bash_wait_detach`: the rest of the background-bash consumer surface the
///   plugins invoke natively (kill a task, drive a PTY, register/remove a
///   watch, detach a wait-mode command when a user message arrives). All are
///   session-scoped task plumbing; the untrusted-bind bash denial still fires
///   first for every `bash_*` name. `bash_kill` and `bash_write` are also agent
///   tools; like `bash_status`, they stay listed here so the plugins' own calls
///   keep being treated as plumbing.
/// - `bash_regex_match`: pure regex compilation and matching for the plugins'
///   `bash_watch` validation and output scan; its parameters are only a regex
///   pattern and text, with no session or configuration privileges.
/// - `bash_artifact_owned`: read-only yes/no answer to "is this path one of
///   the requesting session's own background-bash output files?", which the
///   plugins ask before raising an external-directory prompt for a read. It
///   reads no file and grants nothing; the answer is scoped to the session.
/// - `inspect_tier2_run`: the plugins' background Tier-2 refresh trigger for
///   the bound root; scan work runs on the maintenance class either way.
/// - `hashline_preflight`: parse-only, zero-mutation permission preflight for
///   the session's enabled hashline edit surface; it returns affected paths
///   before the plugin requests edit permission.
pub(crate) fn is_subc_native_plumbing_tool(name: &str) -> bool {
    matches!(
        name,
        "bash_abort_inflight"
            | "bash_status"
            | "bash_drain_completions"
            | "bash_ack_completions"
            | "undo_preview"
            | "checkpoint_paths"
            | "bash_kill"
            | "bash_write"
            | "bash_notify"
            | "bash_unnotify"
            | "bash_wait_detach"
            | "bash_regex_match"
            | "bash_artifact_owned"
            | "inspect_tier2_run"
            | "hashline_preflight"
    )
}

pub(super) fn command_lane_explicit(command: &str) -> Option<Lane> {
    match command {
        "ping"
        | "version"
        | "echo"
        | "bash_drain_completions"
        | "bash_regex_match"
        | "bash_artifact_owned"
        | "bash_wait_detach"
        | "db_get_state"
        | "db_get_host_state"
        | "read"
        | "undo_preview"
        | "edit_history"
        | "checkpoint_paths"
        | "list_checkpoints"
        | "hashline_preflight"
        | "conflicts"
        | "glob"
        | "grep"
        | "git_conflicts"
        | "ast_search" => Some(Lane::PureRead),

        // Lazy reads mutate parser/terminal/url caches on a miss, but are still
        // classified onto the reader pool; install races are handled at the
        // individual cache sites.
        "bash_status" | "outline" | "zoom" => Some(Lane::PureRead),

        "status"
        | "inspect"
        | "lsp_diagnostics"
        | "lsp_inspect"
        | "lsp_hover"
        | "lsp_goto_definition"
        | "lsp_find_references"
        | "lsp_prepare_rename" => Some(Lane::SerialLspStatus),

        "semantic_search" | "search" | "callgraph" | "callers" | "impact" | "call_tree"
        | "trace_to" | "trace_to_symbol" | "trace_data" | "inspect_tier2_run" => {
            Some(Lane::HeavyInit)
        }

        "bash"
        | "powershell"
        | "bash_abort_inflight"
        | "bash_ack_completions"
        | "bash_notify"
        | "bash_unnotify"
        | "bash_promote"
        | "bash_kill"
        | "bash_write"
        | "db_set_state"
        | "db_set_host_state"
        | "undo"
        | "checkpoint"
        | "restore_checkpoint"
        | "write"
        | "apply_patch"
        | "delete_file"
        | "delete"
        | "move_file"
        | "move"
        | "edit"
        | "edit_symbol"
        | "edit_match"
        | "batch"
        | "add_import"
        | "import"
        | "remove_import"
        | "organize_imports"
        | "configure"
        | "move_symbol"
        | "extract_function"
        | "inline_symbol"
        | "ast_replace"
        | "safety"
        | "lsp_rename"
        | "list_filters"
        | "trust_filter_project"
        | "untrust_filter_project"
        | "snapshot" => Some(Lane::Mutating),

        _ => None,
    }
}

pub(super) fn command_lane(command: &str) -> Lane {
    command_lane_explicit(command).unwrap_or(Lane::Mutating)
}

static SUBC_TOOL_SCHEMAS: LazyLock<serde_json::Map<String, Value>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../subc_tool_schemas.json"))
        .unwrap_or_else(|e| panic!("subc_tool_schemas.json: {e}"))
});

/// JSON Schema extension key the generator
/// (`packages/opencode-plugin/src/subc-tool-schemas.ts`) puts on a property
/// that AFT's own plugins set and the model never should. The marker lives on
/// the property itself, so the generator is the only place that decides which
/// properties are consumer-only; the manifest strips every marked property
/// before serving, and the runtime still reads the values when a plugin sends
/// them.
const CONSUMER_ONLY_MARKER: &str = "x-aft-consumer-only";

fn is_consumer_only(property: &Value) -> bool {
    property.get(CONSUMER_ONLY_MARKER).and_then(Value::as_bool) == Some(true)
}

/// Removes consumer-only properties (and their `required` entries) from a
/// tool schema so a consumer handing the catalog straight to a model does not
/// invite the model to set them.
fn strip_consumer_only_properties(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let mut removed = Vec::new();
    if let Some(Value::Object(properties)) = object.get_mut("properties") {
        properties.retain(|name, property| {
            let keep = !is_consumer_only(property);
            if !keep {
                removed.push(name.clone());
            }
            keep
        });
    }
    if let Some(Value::Array(required)) = object.get_mut("required") {
        required.retain(|name| {
            name.as_str()
                .is_none_or(|name| !removed.iter().any(|removed| removed == name))
        });
    }
}

fn tool_schema(name: &str) -> Value {
    let mut schema = SUBC_TOOL_SCHEMAS.get(name).cloned().unwrap_or_else(|| {
        log::warn!(
            "subc build_manifest: missing embedded schema for tool {name:?}; using placeholder"
        );
        json!({ "type": "object" })
    });
    strip_consumer_only_properties(&mut schema);
    schema
}

fn tool_description(name: &str) -> Option<String> {
    SUBC_TOOL_SCHEMAS
        .get(name)
        .and_then(|schema| schema.get("description"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// AFT's subc-mode capability manifest. It uses bare internal tool names
/// because the gateway adds any `aft_` prefix for agent-facing displays; AFT
/// schedules concurrent calls itself; the gateway runs AFT directly without a
/// sandbox. The manifest lists every tool an agent can call over subc.
pub(super) fn build_manifest() -> ModuleManifest {
    build_manifest_for_host(crate::bash_background::powershell_available())
}

/// Builds the manifest for a host where PowerShell is or is not runnable.
///
/// The `powershell` tool is advertised only when `pwsh` resolves, checked when
/// the catalog is served rather than baked into the generated schema artifact:
/// a model shown a tool that cannot run on the host will call it and fail. A
/// call that arrives anyway (the tool stays routable) is refused with an error
/// naming the fix rather than silently run under bash.
pub(super) fn build_manifest_for_host(powershell_available: bool) -> ModuleManifest {
    let tool = |name: &str, execution_mode: ExecutionMode| Tool {
        name: name.to_string(),
        description: tool_description(name),
        execution_mode,
        schema: tool_schema(name),
    };
    // execution_mode keys on externally-observable side effects, NOT internal
    // ctx mutation: the readers warm AFT's own index/cache/symbol artifacts
    // (internal), not the user's workspace, so they are Pure. Bash is Mutating
    // because spawning a detached process changes external state, and edit/write
    // produce observable file writes. Unfenceable stays unused here because AFT
    // schedules bash internally and releases the Mutating worker after spawn.
    //
    // AFT registers not-ready: the daemon answers new route opens with
    // `module_warming` until `subc::readiness` flips it with a
    // `catalog.update { ready: true }`, which it always does within a fixed
    // budget. A daemon that predates the field ignores it and treats the
    // module as ready, which is the behaviour before readiness existed.
    //
    // The builder leaves `capabilities`, `self_signals` and `provenance`
    // unset, so none of them reaches the wire. `consumes` is descriptive (the
    // daemon doesn't read it) and lists the modules AFT opens routes to: the
    // fleet status holder always, and synapse only when it is the configured
    // embedding backend. The manifest is static, so the config-dependent synapse
    // route is declared unconditionally. Neither is a `capabilities.requires`
    // entry, because a required capability with no provider would hold AFT
    // not-ready. A subc daemon older than 0.17.20 refuses this manifest, so
    // that version is the floor.
    ModuleManifest::builder("aft", env!("CARGO_PKG_VERSION"))
        .protocol_ver(PROTOCOL_VERSION)
        .trust_tier(Some(TrustTier::FirstParty))
        .ready(false)
        .provides(vec![
            ProviderRole::ToolProvider {
                tools: [
                    Some(tool("status", ExecutionMode::Pure)),
                    Some(tool("bash", ExecutionMode::Mutating)),
                    powershell_available.then(|| tool("powershell", ExecutionMode::Mutating)),
                ]
                .into_iter()
                .flatten()
                .chain([
                    tool("read", ExecutionMode::Pure),
                    tool("write", ExecutionMode::Mutating),
                    tool("edit", ExecutionMode::Mutating),
                    tool("apply_patch", ExecutionMode::Mutating),
                    tool("grep", ExecutionMode::Pure),
                    tool("glob", ExecutionMode::Pure),
                    tool("search", ExecutionMode::Pure),
                    tool("outline", ExecutionMode::Pure),
                    tool("zoom", ExecutionMode::Pure),
                    tool("inspect", ExecutionMode::Pure),
                    tool("callgraph", ExecutionMode::Pure),
                    tool("conflicts", ExecutionMode::Pure),
                    tool("ast_search", ExecutionMode::Pure),
                    tool("ast_replace", ExecutionMode::Mutating),
                    tool("delete", ExecutionMode::Mutating),
                    tool("move", ExecutionMode::Mutating),
                    tool("import", ExecutionMode::Mutating),
                    tool("safety", ExecutionMode::Mutating),
                    // Companions for the task ids `bash`/`powershell` hand back
                    // (explicit background, PTY, promotion after the wait
                    // window, detach on restart). The bash reply text tells
                    // the model to call them, so a consumer that builds its
                    // tool surface from this catalog must be offered them.
                    // There is no `bash_watch` here: that waiting loop is
                    // implemented inside the OpenCode and Pi plugins, not by
                    // the module, and the catalog text does not mention it.
                    tool("bash_status", ExecutionMode::Pure),
                    tool("bash_kill", ExecutionMode::Mutating),
                    tool("bash_write", ExecutionMode::Mutating),
                ])
                .collect(),
                identity_scope: vec![IdentityScope::Session, IdentityScope::Project],
                concurrency: Concurrency::ModuleManaged,
                emits_push: true,
                sub_supervises: true,
            },
            // subc-protocol defines management operations as a name plus a
            // Query/Mutate kind. These queries carry their own optional root
            // input and do not scope the route itself.
            ProviderRole::ManagementSurface {
                operations: vec![
                    ManagementOperation {
                        name: crate::commands::health_digest::HEALTH_DIGEST_OPERATION.to_string(),
                        kind: ManagementOperationKind::Query,
                        description: None,
                    },
                    ManagementOperation {
                        name: crate::commands::memory_census::MEMORY_CENSUS_OPERATION.to_string(),
                        kind: ManagementOperationKind::Query,
                        description: None,
                    },
                    ManagementOperation {
                        name: crate::commands::writes_census::WRITES_CENSUS_OPERATION.to_string(),
                        kind: ManagementOperationKind::Query,
                        description: None,
                    },
                    // The gh shim's bot-write relay. It is plumbing, not an agent
                    // tool: the caller's per-command ticket is its only
                    // authority (see `crate::gh_shim_relay`).
                    ManagementOperation {
                        name: crate::gh_shim_relay::BOT_REQUEST_OPERATION.to_string(),
                        kind: ManagementOperationKind::Mutate,
                        description: None,
                    },
                    ManagementOperation {
                        name: crate::gh_shim_relay::BINDINGS_READ_OPERATION.to_string(),
                        kind: ManagementOperationKind::Query,
                        description: None,
                    },
                ],
                config_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
                observability: Vec::new(),
                identity_scope: Vec::new(),
                // Explicitly the value the daemon assumes when this field is
                // absent: management calls keep the concurrent delivery they
                // received before the protocol could express the choice.
                concurrency: Concurrency::ModuleManaged,
            },
        ])
        .bindings(Some(Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: true,
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: vec![IdentityScope::Project],
                optional: vec![IdentityScope::Session],
            },
        }))
        .consumes(vec![
            ConsumerRole::ServiceClient {
                of: vec!["prefrontal-core".to_string()],
            },
            ConsumerRole::ServiceClient {
                of: vec!["synapse".to_string()],
            },
        ])
        .build()
}

pub(super) fn control_ops() -> Option<Vec<String>> {
    Some(vec![
        "route.bind".to_string(),
        "route.status".to_string(),
        MODULE_CONTROL_OP_HEALTH_CHECK.to_string(),
    ])
}

pub(super) fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subc_translate::supports_tool;
    use std::collections::{HashMap, HashSet};

    const CORE_TOOLS: &[&str] = &[
        "status",
        "bash",
        "powershell",
        "read",
        "write",
        "edit",
        "apply_patch",
        "grep",
        "glob",
        "search",
        "outline",
        "zoom",
        "inspect",
        "callgraph",
        "conflicts",
        "ast_search",
        "ast_replace",
        "delete",
        "move",
        "import",
        "safety",
        "bash_status",
        "bash_kill",
        "bash_write",
    ];

    /// Tools listed here deliberately skip translation; adding one is a reviewed
    /// decision because it weakens the registration guard's translation check.
    const TRANSLATION_EXEMPT: &[&str] = &[];

    fn is_bare_placeholder_schema(schema: &Value) -> bool {
        schema == &json!({ "type": "object" })
    }

    #[test]
    fn build_manifest_serves_embedded_tool_schemas() {
        let manifest = build_manifest_for_host(true);
        let tools = match manifest.provides.first() {
            Some(ProviderRole::ToolProvider { tools, .. }) => tools,
            _ => panic!("expected ToolProvider"),
        };
        let by_name: HashMap<&str, &Tool> = tools.iter().map(|t| (t.name.as_str(), t)).collect();
        for name in CORE_TOOLS {
            let tool = by_name
                .get(name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            assert!(
                tool.description
                    .as_deref()
                    .is_some_and(|description| !description.is_empty()),
                "{name} must carry a non-empty manifest description"
            );
            assert!(
                !is_bare_placeholder_schema(&tool.schema),
                "{name} must not use bare placeholder schema"
            );
            assert_eq!(
                tool.schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "{name} schema must be an object"
            );
        }

        // The module manifest is shared across trusted and untrusted routes.
        // Keep its committed, default-gate description free of GitHub resource
        // spellings because untrusted consumers are never granted that feature.
        assert!(!by_name["read"]
            .description
            .as_deref()
            .unwrap_or_default()
            .contains("issue://NUMBER"));

        let read = by_name["read"]
            .schema
            .get("properties")
            .and_then(|p| p.as_object());
        let read_props = read.expect("read schema properties");
        // The manifest is generated from the OpenCode tool map, where the
        // hoisted read/write/edit trio advertises `filePath` to satisfy the
        // host's file-header display contract (the UI renders the recorded
        // model input verbatim). `path` stays canonical everywhere else and is
        // still accepted at runtime.
        assert!(
            read_props.contains_key("filePath"),
            "read schema must expose the hoisted trio's filePath"
        );

        let status = &by_name["status"].schema;
        assert_eq!(
            status.get("properties").and_then(|v| v.as_object()),
            Some(&serde_json::Map::new()),
            "status schema must have empty properties"
        );
        assert_eq!(
            status.get("additionalProperties").and_then(|v| v.as_bool()),
            Some(false),
            "status schema must forbid additionalProperties"
        );
    }

    #[test]
    fn build_manifest_declares_management_queries_outside_agent_tools() {
        let manifest = build_manifest_for_host(true);
        let tools = manifest
            .provides
            .iter()
            .find_map(|role| match role {
                ProviderRole::ToolProvider { tools, .. } => Some(tools),
                _ => None,
            })
            .expect("tool provider role");
        let operations = manifest
            .provides
            .iter()
            .find_map(|role| match role {
                ProviderRole::ManagementSurface { operations, .. } => Some(operations),
                _ => None,
            })
            .expect("management surface role");

        assert_eq!(
            operations
                .iter()
                .map(|operation| (operation.name.as_str(), &operation.kind))
                .collect::<Vec<_>>(),
            vec![
                (
                    crate::commands::health_digest::HEALTH_DIGEST_OPERATION,
                    &ManagementOperationKind::Query,
                ),
                (
                    crate::commands::memory_census::MEMORY_CENSUS_OPERATION,
                    &ManagementOperationKind::Query,
                ),
                (
                    crate::commands::writes_census::WRITES_CENSUS_OPERATION,
                    &ManagementOperationKind::Query,
                ),
                (
                    crate::gh_shim_relay::BOT_REQUEST_OPERATION,
                    &ManagementOperationKind::Mutate,
                ),
                (
                    crate::gh_shim_relay::BINDINGS_READ_OPERATION,
                    &ManagementOperationKind::Query,
                ),
            ]
        );
        for operation in operations {
            assert!(
                tools.iter().all(|tool| tool.name != operation.name),
                "management operation {} must not enter the agent tool manifest",
                operation.name
            );
            assert!(
                !SUBC_TOOL_SCHEMAS.contains_key(&operation.name),
                "management operation {} must not enter the subc tool schema artifact",
                operation.name
            );
        }
        assert!(!control_ops()
            .expect("control operations")
            .iter()
            .any(|operation| operation == crate::commands::memory_census::MEMORY_CENSUS_OPERATION));
        assert!(!control_ops()
            .expect("control operations")
            .iter()
            .any(|operation| operation == crate::commands::writes_census::WRITES_CENSUS_OPERATION));
    }

    #[test]
    fn embedded_subc_tools_are_registered_across_all_rust_surfaces() {
        let schema_names: HashSet<&str> = SUBC_TOOL_SCHEMAS.keys().map(String::as_str).collect();
        let core_names: HashSet<&str> = CORE_TOOLS.iter().copied().collect();
        assert_eq!(
            CORE_TOOLS.len(),
            schema_names.len(),
            "CORE_TOOLS count must match embedded schema key count"
        );
        assert_eq!(
            core_names, schema_names,
            "CORE_TOOLS must exactly match embedded schema keys"
        );

        let manifest = build_manifest_for_host(true);
        let tools = match manifest.provides.first() {
            Some(ProviderRole::ToolProvider { tools, .. }) => tools,
            _ => panic!("expected ToolProvider"),
        };
        let manifest_names: HashSet<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();

        for name in schema_names {
            assert!(
                is_subc_agent_core_tool(name),
                "tool {name:?} is missing from is_subc_agent_core_tool in crates/aft/src/subc/manifest.rs"
            );
            assert!(
                manifest_names.contains(name),
                "tool {name:?} is missing from build_manifest in crates/aft/src/subc/manifest.rs"
            );
            assert!(
                command_lane_explicit(name).is_some(),
                "tool {name:?} is missing an explicit command_lane arm in crates/aft/src/subc/manifest.rs"
            );
            if !TRANSLATION_EXEMPT.contains(&name) {
                assert!(
                    supports_tool(name),
                    "tool {name:?} is missing from supports_tool in crates/aft/src/subc_translate.rs"
                );
            }
        }

        // BARE_TOOL_ORDER is TypeScript-only; the embedded schema map is its
        // generated Rust-side artifact, so the manifest count is the Rust
        // denominator check for this derived guard.
        assert_eq!(
            SUBC_TOOL_SCHEMAS.len(),
            tools.len(),
            "registration guard denominator must match manifest tool count"
        );
    }

    #[test]
    fn build_manifest_classifies_execution_mode_by_observable_effect() {
        let manifest = build_manifest_for_host(true);
        let tools = match manifest.provides.first() {
            Some(ProviderRole::ToolProvider { tools, .. }) => tools,
            _ => panic!("expected ToolProvider"),
        };
        let by_name: HashMap<&str, &Tool> = tools.iter().map(|t| (t.name.as_str(), t)).collect();

        // Readers warm AFT's own index/cache/symbol artifacts (internal ctx
        // mutation), not the user's observable workspace, so they are Pure.
        for name in [
            "status",
            "read",
            "grep",
            "glob",
            "search",
            "outline",
            "zoom",
            "inspect",
            "callgraph",
            "conflicts",
            "ast_search",
            "bash_status",
        ] {
            assert_eq!(
                by_name[name].execution_mode,
                ExecutionMode::Pure,
                "{name} produces no observable side effect and must be Pure"
            );
        }
        // Mutating tools can write files, change safety state, or spawn processes.
        for name in [
            "bash",
            "powershell",
            "write",
            "edit",
            "apply_patch",
            "ast_replace",
            "delete",
            "move",
            "import",
            "safety",
            "bash_kill",
            "bash_write",
        ] {
            assert_eq!(
                by_name[name].execution_mode,
                ExecutionMode::Mutating,
                "{name} writes files and must be Mutating"
            );
        }
    }

    /// Serializes the HELLO manifest with the two volatile parts replaced by
    /// markers: the crate version (changes every release) and each tool's
    /// embedded schema/description (regenerated from the plugin tool map).
    /// Both are checked against their sources before being replaced, so the
    /// snapshot still pins everything else AFT puts on the wire.
    fn normalized_manifest_json() -> Value {
        let mut manifest =
            serde_json::to_value(build_manifest_for_host(true)).expect("serialize manifest");
        assert_eq!(manifest["module_version"], json!(env!("CARGO_PKG_VERSION")));
        manifest["module_version"] = json!("<CARGO_PKG_VERSION>");
        let tools = manifest["provides"][0]["tools"]
            .as_array_mut()
            .expect("tool provider tools");
        for tool in tools {
            let name = tool["name"].as_str().expect("tool name").to_string();
            assert_eq!(tool["schema"], tool_schema(&name), "{name} schema");
            assert_eq!(
                tool["description"],
                json!(tool_description(&name)),
                "{name} description"
            );
            tool["schema"] = json!("<embedded schema>");
            tool["description"] = json!("<embedded description>");
        }
        manifest
    }

    /// The fixture is the manifest AFT sent on subc-protocol 0.10, captured
    /// before the move to 0.22. Every difference the newer protocol crate
    /// forces is applied to it explicitly below, so any other drift in what
    /// AFT sends fails this test.
    ///
    /// The two removed keys are real wire differences, not equivalences: a
    /// subc daemon older than 0.17.20 requires them and refuses the HELLO
    /// ("missing field `consumes`"), so this manifest needs daemon 0.17.20 or
    /// newer.
    #[test]
    fn hello_manifest_wire_shape_matches_snapshot() {
        let actual = normalized_manifest_json();
        let mut expected: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/subc_hello_manifest.json"
        ))
        .expect("parse manifest snapshot");
        let top = expected.as_object_mut().expect("manifest object");
        // `consumes` names the modules AFT opens service routes to.
        assert_eq!(top.remove("consumes"), Some(json!([])));
        top.insert(
            "consumes".to_string(),
            json!([
                {"role": "service_client", "of": ["prefrontal-core"]},
                {"role": "service_client", "of": ["synapse"]}
            ]),
        );
        // The scheduled-task vocabulary was retired from the manifest.
        assert_eq!(top.remove("scheduled_tasks"), Some(json!([])));
        // The management role now always serializes its delivery concurrency;
        // AFT declares the value the daemon assumes when the key is absent.
        let management = expected["provides"][1]
            .as_object_mut()
            .expect("management surface role");
        assert_eq!(management["role"], json!("management_surface"));
        assert_eq!(
            management.insert("concurrency".to_string(), json!("module_managed")),
            None
        );
        // The snapshot predates the gh shim relay operations, so they are
        // appended to its management operation list here.
        management
            .get_mut("operations")
            .and_then(Value::as_array_mut)
            .expect("management operations")
            .extend([
                json!({"name": crate::gh_shim_relay::BOT_REQUEST_OPERATION, "kind": "mutate"}),
                json!({"name": crate::gh_shim_relay::BINDINGS_READ_OPERATION, "kind": "query"}),
            ]);
        // AFT registers not-ready and flips itself ready after warm-up (see
        // `subc::readiness`); this is the only field readiness adds.
        let top = expected.as_object_mut().expect("manifest object");
        assert_eq!(top.insert("ready".to_string(), json!(false)), None);
        // The snapshot predates the bash companion tools, which the catalog
        // appends after `safety`.
        expected["provides"][0]["tools"]
            .as_array_mut()
            .expect("tool provider tools")
            .extend(
                [
                    ("bash_status", "pure"),
                    ("bash_kill", "mutating"),
                    ("bash_write", "mutating"),
                ]
                .map(|(name, mode)| {
                    json!({
                        "name": name,
                        "description": "<embedded description>",
                        "execution_mode": mode,
                        "schema": "<embedded schema>",
                    })
                }),
            );
        assert_eq!(actual, expected);
    }

    #[test]
    fn subc_agent_lanes_classify_new_read_tools() {
        assert_eq!(command_lane("callgraph"), Lane::HeavyInit);
        assert_eq!(command_lane("conflicts"), Lane::PureRead);
        assert_eq!(command_lane("bash_status"), Lane::PureRead);
        assert_eq!(command_lane("bash_wait_detach"), Lane::PureRead);
        assert!(is_subc_native_plumbing_tool("bash_status"));
    }

    #[test]
    fn native_plumbing_allowlist_admits_exactly_the_plugin_consumer_surface() {
        // BC2: the route gate admits a name when it's an agent core tool OR a
        // native plumbing command. These carry no agent surface and no
        // config/trust surface, so they're admitted to dispatch over a bound
        // route while everything else (notably `configure`) stays fail-closed.
        assert!(is_subc_native_plumbing_tool("bash_drain_completions"));
        assert!(is_subc_native_plumbing_tool("bash_ack_completions"));
        // Safety-tool permission previews: read-only, session-scoped. Without
        // these, aft_safety undo/restore breaks over the subc transport.
        assert!(is_subc_native_plumbing_tool("undo_preview"));
        assert!(is_subc_native_plumbing_tool("checkpoint_paths"));
        // The rest of the plugins' background-bash consumer surface plus the
        // Tier-2 refresh trigger (each was shipped plugin-side without a gate
        // entry and silently rejected in prod; see subc_plumbing_drift_test).
        assert!(is_subc_native_plumbing_tool("bash_kill"));
        assert!(is_subc_native_plumbing_tool("bash_write"));
        assert!(is_subc_native_plumbing_tool("bash_notify"));
        assert!(is_subc_native_plumbing_tool("bash_unnotify"));
        assert!(is_subc_native_plumbing_tool("bash_wait_detach"));
        // Regex validation is session-scoped plumbing with no config/trust input.
        assert!(is_subc_native_plumbing_tool("bash_regex_match"));
        assert!(is_subc_native_plumbing_tool("inspect_tier2_run"));
        // Hashline preflight parses the patch and reports permission paths; it
        // does not mutate files or expose configuration or trust controls.
        assert!(is_subc_native_plumbing_tool("hashline_preflight"));

        // The allowlist is TIGHT — it must not admit the config-bypass vector
        // the fail-closed gate exists to block, nor mutation commands the
        // plugins never send natively.
        assert!(!is_subc_native_plumbing_tool("configure"));
        assert!(!is_subc_native_plumbing_tool("bash"));
        assert!(!is_subc_native_plumbing_tool("db_set_state"));
        assert!(!is_subc_native_plumbing_tool("undo"));

        // The plumbing commands are NOT agent-facing tools — they must stay out
        // of the manifest gate so they never reach the model surface.
        assert!(!is_subc_agent_core_tool("bash_drain_completions"));
        assert!(!is_subc_agent_core_tool("bash_ack_completions"));
        assert!(!is_subc_agent_core_tool("hashline_preflight"));
        assert!(!is_subc_agent_core_tool("bash_regex_match"));

        // Parse-only preflight and completion drain are reads; ack mutates.
        assert_eq!(command_lane("hashline_preflight"), Lane::PureRead);
        assert_eq!(command_lane("bash_drain_completions"), Lane::PureRead);
        assert_eq!(command_lane("bash_ack_completions"), Lane::Mutating);
    }

    /// Every property name the generator marked consumer-only, per tool, read
    /// from the raw embedded artifact before any stripping.
    fn marked_consumer_only_properties() -> Vec<(String, String)> {
        let mut marked = Vec::new();
        for (tool, schema) in SUBC_TOOL_SCHEMAS.iter() {
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (name, property) in properties {
                    if is_consumer_only(property) {
                        marked.push((tool.clone(), name.clone()));
                    }
                }
            }
        }
        marked
    }

    /// Collects every `properties` entry at any depth of a served schema.
    fn collect_properties<'a>(schema: &'a Value, out: &mut Vec<(&'a str, &'a Value)>) {
        match schema {
            Value::Object(object) => {
                if let Some(Value::Object(properties)) = object.get("properties") {
                    for (name, property) in properties {
                        out.push((name.as_str(), property));
                    }
                }
                for value in object.values() {
                    collect_properties(value, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect_properties(item, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn served_catalog_carries_no_consumer_only_property() {
        let marked = marked_consumer_only_properties();
        // Guards against a vacuous pass: the plugins' bash flags must still be
        // marked in the artifact, or there would be nothing to strip.
        for expected in ["foreground_orchestrate", "block_to_completion", "shell"] {
            assert!(
                marked
                    .iter()
                    .any(|(tool, name)| tool == "bash" && name == expected),
                "bash.{expected} must carry {CONSUMER_ONLY_MARKER} in subc_tool_schemas.json"
            );
        }

        for powershell_available in [true, false] {
            let manifest = build_manifest_for_host(powershell_available);
            let tools = match manifest.provides.first() {
                Some(ProviderRole::ToolProvider { tools, .. }) => tools,
                _ => panic!("expected ToolProvider"),
            };
            for tool in tools {
                let mut properties = Vec::new();
                collect_properties(&tool.schema, &mut properties);
                for (name, property) in properties {
                    assert!(
                        !is_consumer_only(property),
                        "{}.{name} is marked consumer-only but is served to consumers",
                        tool.name
                    );
                    assert!(
                        !marked
                            .iter()
                            .any(|(marked_tool, marked_name)| marked_tool == &tool.name
                                && marked_name == name),
                        "{}.{name} is consumer-only but is served to consumers",
                        tool.name
                    );
                    // Backstop for a consumer flag added without the marker:
                    // the generator describes these as "Consumer-set".
                    assert!(
                        !property
                            .get("description")
                            .and_then(Value::as_str)
                            .is_some_and(|description| description.contains("Consumer-set")),
                        "{}.{name} describes itself as consumer-set but is served to consumers",
                        tool.name
                    );
                }
                let required = tool
                    .schema
                    .get("required")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for (marked_tool, marked_name) in &marked {
                    assert!(
                        !(marked_tool == &tool.name
                            && required.iter().any(|name| name == marked_name.as_str())),
                        "{}.{marked_name} is consumer-only but still listed as required",
                        tool.name
                    );
                }
            }
        }
    }

    #[test]
    fn powershell_is_advertised_only_where_pwsh_can_run() {
        let names = |powershell_available: bool| -> Vec<String> {
            match build_manifest_for_host(powershell_available)
                .provides
                .first()
            {
                Some(ProviderRole::ToolProvider { tools, .. }) => {
                    tools.iter().map(|tool| tool.name.clone()).collect()
                }
                _ => panic!("expected ToolProvider"),
            }
        };
        let without = names(false);
        assert!(
            !without.iter().any(|name| name == "powershell"),
            "a host without pwsh must not advertise the powershell tool: {without:?}"
        );
        assert!(without.iter().any(|name| name == "bash"));
        assert_eq!(without.len(), CORE_TOOLS.len() - 1);

        let with = names(true);
        assert!(with.iter().any(|name| name == "powershell"));
        assert_eq!(with.len(), CORE_TOOLS.len());

        // Hiding the tool does not unroute it: a stray call still reaches the
        // executor, which refuses it with an install-or-use-bash error.
        assert!(is_subc_agent_core_tool("powershell"));

        // The served default follows the host's real pwsh lookup.
        let served = match build_manifest().provides.first() {
            Some(ProviderRole::ToolProvider { tools, .. }) => {
                tools.iter().any(|tool| tool.name == "powershell")
            }
            _ => panic!("expected ToolProvider"),
        };
        assert_eq!(served, crate::bash_background::powershell_available());
    }

    /// Every `bash_*` identifier in `text` (e.g. `bash_status` in
    /// "use bash_status({ taskId })"). Config keys are spelled with a dot
    /// (`bash.watch_sync_max_ms`), so they are not picked up.
    fn bash_companion_mentions(text: &str) -> HashSet<String> {
        let mut names = HashSet::new();
        let bytes = text.as_bytes();
        let mut start = 0;
        while let Some(offset) = text[start..].find("bash_") {
            let at = start + offset;
            let preceded_by_word =
                at > 0 && (bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
            let end = text[at..]
                .find(|c: char| !(c.is_ascii_lowercase() || c == '_'))
                .map_or(text.len(), |len| at + len);
            if !preceded_by_word {
                names.insert(text[at..end].trim_end_matches('_').to_string());
            }
            start = end.max(at + 1);
        }
        names
    }

    /// Reply text the server renders for the bash family and its companions,
    /// i.e. what a catalog consumer shows the model after a `bash`/`powershell`
    /// call hands back a task id and the model follows up on it.
    fn server_rendered_bash_reply_texts() -> Vec<String> {
        use crate::commands::bash_orchestrate as orchestrate;
        let task = "bash-0123456789abcdef";
        let running_status = |mode: &str| {
            crate::subc_format::format_response(
                "bash_status",
                &crate::protocol::Response::success(
                    "status",
                    json!({ "task_id": task, "status": "running", "mode": mode }),
                ),
                false,
            )
        };
        vec![
            orchestrate::format_background_launch(task, false),
            orchestrate::format_background_launch(task, true),
            orchestrate::format_promotion_message(task, None, 30_000),
            orchestrate::format_wait_detach_message(task),
            orchestrate::format_module_drain_detach_message(task),
            running_status("pty"),
            running_status("pipes"),
        ]
    }

    /// A consumer that builds its tool surface from this catalog can only offer
    /// the model what the catalog lists. Whenever `bash` or `powershell` is
    /// advertised, every companion tool named by any advertised description,
    /// property description, or server-rendered bash reply must be advertised
    /// too; otherwise the model is told to call a tool its host refuses.
    #[test]
    fn catalog_advertises_every_bash_companion_its_text_names() {
        for powershell_available in [true, false] {
            let manifest = build_manifest_for_host(powershell_available);
            let tools = match manifest.provides.first() {
                Some(ProviderRole::ToolProvider { tools, .. }) => tools,
                _ => panic!("expected ToolProvider"),
            };
            let advertised: HashSet<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
            if !advertised.contains("bash") && !advertised.contains("powershell") {
                continue;
            }

            let mut sources: Vec<(String, String)> = Vec::new();
            for tool in tools {
                if let Some(description) = &tool.description {
                    sources.push((format!("{} description", tool.name), description.clone()));
                }
                let mut properties = Vec::new();
                collect_properties(&tool.schema, &mut properties);
                for (name, property) in properties {
                    if let Some(description) = property.get("description").and_then(Value::as_str) {
                        sources.push((
                            format!("{}.{name} description", tool.name),
                            description.to_string(),
                        ));
                    }
                }
            }
            for text in server_rendered_bash_reply_texts() {
                sources.push(("server-rendered bash reply".to_string(), text));
            }

            let mut referenced = HashSet::new();
            let mut missing = Vec::new();
            for (source, text) in &sources {
                for name in bash_companion_mentions(text) {
                    if !advertised.contains(name.as_str()) {
                        missing.push(format!("{name} (named by {source})"));
                    }
                    referenced.insert(name);
                }
            }
            missing.sort();
            missing.dedup();
            assert!(
                missing.is_empty(),
                "catalog (powershell_available={powershell_available}) names bash companions it does not advertise: {missing:?}"
            );
            // Guards against a vacuous pass: the launch text alone names
            // bash_status, bash_kill and bash_write.
            for expected in ["bash_status", "bash_kill", "bash_write"] {
                assert!(
                    referenced.contains(expected),
                    "scan found no mention of {expected}; the text sources are not being read"
                );
            }
        }
    }

    #[test]
    fn consumer_only_bash_flags_still_reach_the_runtime() {
        // Our plugins send these flags explicitly; stripping them from the
        // served schema must not stop translation from carrying them through.
        let translated = crate::subc_translate::subc_translate_owned(
            "bash",
            json!({
                "command": "echo hi",
                "foreground_orchestrate": false,
                "block_to_completion": true,
                "shell": "powershell",
            }),
            std::path::Path::new("/project"),
        )
        .expect("bash with consumer flags must translate");
        assert_eq!(translated.command, "bash");
        assert_eq!(translated.args["foreground_orchestrate"], json!(false));
        assert_eq!(translated.args["block_to_completion"], json!(true));
        assert_eq!(translated.args["shell"], json!("powershell"));

        let bash_properties = tool_schema("bash");
        let bash_properties = bash_properties["properties"]
            .as_object()
            .expect("bash properties");
        for flag in ["foreground_orchestrate", "block_to_completion", "shell"] {
            assert!(!bash_properties.contains_key(flag), "bash.{flag} leaked");
        }
    }
}
