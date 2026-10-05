//! The v1 catalog and admission boundary, separate from plugin tool forwarding.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cortexkit_role_tool_provider::{
    call::{check_call, SchemaPin},
    catalog::{
        composition_digest, schema_digest, system_text_digest, CatalogAnswer, CatalogRequest,
        CatalogTool, SystemTextAnswer,
    },
    describe::{Major, RoleDescribe},
    errors,
};
use serde_json::{json, Value};
use subc_protocol::ErrorBody;

use super::manifest;

#[cfg(test)]
thread_local! {
    static ADMISSION_WORK: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RouteRole {
    Legacy,
    ToolProviderV1,
}

/// Only an explicit bind declaration selects v1; a call's pins and harness do not.
/// The daemon forwards the consumer's route-open `role_versions` unchanged.
pub(super) fn route_role(
    versions: Option<&BTreeMap<String, String>>,
) -> Result<RouteRole, ErrorBody> {
    match versions.and_then(|versions| versions.get("tool-provider")) {
        None => Ok(RouteRole::Legacy),
        Some(version) if version == "v1" => Ok(RouteRole::ToolProviderV1),
        Some(_) => Err(ErrorBody::new(
            "unsupported_role_version",
            "AFT serves tool-provider versions [\"v1\"]",
        )
        .with_detail(json!({"versions": ["v1"]}))),
    }
}

pub(super) fn recognized_operation(op: &str) -> bool {
    matches!(
        op,
        "role.describe"
            | "tool.catalog"
            | "tool.call"
            | "tool.withdraw"
            | "late_results"
            | "late_results.ack"
    )
}

pub(super) fn describe() -> Value {
    serde_json::to_value(RoleDescribe {
        majors: vec![Major {
            version: "tool-provider/v1".into(),
            ops: vec![
                "role.describe".into(),
                "tool.catalog".into(),
                "tool.call".into(),
            ],
            stability: "alpha".into(),
        }],
        implementation_version: env!("CARGO_PKG_VERSION").into(),
        capabilities: vec![],
    })
    .expect("the commons role declaration is JSON")
}

// Each tool has explicit tags and result permissions: read and bash prohibit
// replacement of line-addressed output, while powershell permits replacement.
pub(super) const METADATA: &[(&str, &str, bool)] = &[
    ("status", "", false),
    ("bash", "shell.exec/v1", true),
    ("powershell", "shell.exec/v1", false),
    ("read", "code.read/v1", true),
    ("write", "code.edit/v1", false),
    ("edit", "code.edit/v1", false),
    ("apply_patch", "code.edit/v1", false),
    ("grep", "code.search/v1", false),
    ("glob", "code.search/v1", false),
    ("search", "code.search/v1", false),
    ("outline", "code.outline/v1", false),
    ("zoom", "code.outline/v1", false),
    ("inspect", "code.diagnostics/v1", false),
    ("callgraph", "code.callgraph/v1", false),
    ("conflicts", "aft:git.conflicts/v1", false),
    ("ast_search", "aft:code.ast_grep/v1", false),
    ("ast_replace", "aft:code.ast_grep/v1", false),
    ("delete", "code.files/v1", false),
    ("move", "code.files/v1", false),
    ("import", "aft:code.imports/v1", false),
    ("safety", "aft:safety/v1", false),
    ("bash_status", "shell.exec/v1", false),
    ("bash_kill", "shell.exec/v1", false),
    ("bash_write", "shell.exec/v1", false),
];

#[cfg(test)]
pub(super) fn tools(disabled: &[String], powershell_available: bool) -> Vec<CatalogTool> {
    tools_for(CatalogPreset::Head, disabled, powershell_available)
}

/// The named catalog variants AFT serves. A consumer's plan item names one
/// (`tool.catalog` `preset`); an absent preset is `Head`, the catalog AFT
/// served before presets existed. Each preset fixes the tool set, the
/// per-tool descriptions and the system text, so a runner that freezes one
/// answer gets a consistent surface for the role it launched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogPreset {
    /// A primary session: every tool, with the wording the catalog always had.
    Head,
    /// A delegated worker: the head tools plus `bash_watch`, worded for a
    /// session that no completion reminder ever wakes.
    Worker,
    /// A read-only session: only the tools that read or search code.
    Reader,
}

impl CatalogPreset {
    pub(crate) const ALL: [Self; 3] = [Self::Head, Self::Worker, Self::Reader];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Head => "head",
            Self::Worker => "worker",
            Self::Reader => "reader",
        }
    }

    /// Resolves a requested preset. An unknown one is refused by name, never
    /// guessed; `field` is the request member that named it.
    pub(crate) fn parse(field: &str, preset: Option<&str>) -> Result<Self, ErrorBody> {
        match preset {
            None => Ok(Self::Head),
            Some(name) => Self::ALL
                .into_iter()
                .find(|preset| preset.name() == name)
                .ok_or_else(|| {
                    errors::invalid_request(
                        field,
                        format!(
                            "AFT defines no preset {name:?}; its presets are \"head\", \"worker\" and \"reader\""
                        ),
                    )
                }),
        }
    }
}

/// The tools the `reader` preset serves: reading and searching code, nothing
/// that runs a command or changes a file.
pub(crate) const READER_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "search",
    "outline",
    "zoom",
    "callgraph",
    "inspect",
];

/// Metadata for tools only a non-head preset serves. They are not in the
/// module manifest (the HELLO surface and every legacy consumer stay as they
/// were), so they are listed apart from [`METADATA`].
pub(super) const PRESET_ONLY_METADATA: &[(&str, &str, bool)] =
    &[("bash_watch", "shell.exec/v1", false)];

/// The catalog entries `preset` serves, in manifest order, with a worker-only
/// tool placed after the head tool it accompanies.
pub(super) fn tools_for(
    preset: CatalogPreset,
    disabled: &[String],
    powershell_available: bool,
) -> Vec<CatalogTool> {
    served_tools(preset)
        .iter()
        .filter(|tool| powershell_available || tool.catalog.name != "powershell")
        .filter(|tool| crate::tool_gate::catalog_keeps(&tool.catalog.name, disabled))
        .filter(|tool| {
            tool.catalog.name != "bash_watch" || crate::tool_gate::catalog_keeps("bash", disabled)
        })
        .map(|tool| tool.catalog.clone())
        .collect()
}

// Schemas and semantics are embedded in the executable. Host availability and
// disabled-tool policy select from this immutable catalog; they do not change
// its schemas, so neither digests nor validators need request-scoped rebuilding.
struct ServedTool {
    catalog: CatalogTool,
    validator: OnceLock<jsonschema::Validator>,
}

static SERVED_TOOLS: LazyLock<[Vec<ServedTool>; 3]> = LazyLock::new(|| {
    CatalogPreset::ALL.map(|preset| {
        build_tools(preset)
            .into_iter()
            .map(|catalog| ServedTool {
                catalog,
                validator: OnceLock::new(),
            })
            .collect()
    })
});

fn served_tools(preset: CatalogPreset) -> &'static [ServedTool] {
    &SERVED_TOOLS[match preset {
        CatalogPreset::Head => 0,
        CatalogPreset::Worker => 1,
        CatalogPreset::Reader => 2,
    }]
}

fn build_tools(preset: CatalogPreset) -> Vec<CatalogTool> {
    #[cfg(test)]
    ADMISSION_WORK.with(|count| {
        let (catalogs, validators) = count.get();
        count.set((catalogs + 1, validators));
    });
    let manifest = manifest::build_manifest_for_host(true);
    let mut served: Vec<(String, Value, Option<String>)> = manifest
        .provides
        .into_iter()
        .filter_map(|role| match role {
            subc_protocol::manifest::ProviderRole::ToolProvider { tools, .. } => Some(tools),
            _ => None,
        })
        .flatten()
        .map(|tool| (tool.name, tool.schema, tool.description))
        .collect();
    match preset {
        CatalogPreset::Head => {}
        CatalogPreset::Worker => {
            for (name, schema, description) in &mut served {
                if let Some((worker_schema, worker_description)) =
                    manifest::preset_tool(preset.name(), name)
                {
                    *schema = worker_schema;
                    *description = worker_description;
                }
            }
            // `bash_watch` waits on the task ids `bash` hands back, so it sits
            // beside `bash_status` and is served only where its own disable
            // key and `bash` both leave it reachable.
            if served.iter().any(|(name, _, _)| name == "bash") {
                let (schema, description) = manifest::preset_tool(preset.name(), "bash_watch")
                    .expect("the worker preset artifact carries bash_watch");
                let at = served
                    .iter()
                    .position(|(name, _, _)| name == "bash_status")
                    .map_or(served.len(), |index| index + 1);
                served.insert(at, ("bash_watch".into(), schema, description));
            }
        }
        CatalogPreset::Reader => {
            served.retain(|(name, _, _)| READER_TOOLS.contains(&name.as_str()));
        }
    }
    served
        .into_iter()
        .map(|(name, mut schema, description)| {
            let (_, tag, restrict_replace) = METADATA
                .iter()
                .chain(PRESET_ONLY_METADATA)
                .find(|(known, _, _)| *known == name)
                .expect("every served tool has v1 metadata");
            schema
                .as_object_mut()
                .expect("tool schemas are objects")
                .remove("description");
            let digest = schema_digest(&schema).expect("embedded schema has a structural digest");
            let mut entry = CatalogTool::new(name, digest, 1, schema);
            if !tag.is_empty() {
                entry.capabilities.push((*tag).into());
            }
            entry.description = description;
            if *restrict_replace {
                entry.result_ops = Some(vec!["prepend".into(), "append".into()]);
            }
            entry
        })
        .collect()
}

/// The lines every preset's system text shares, one per served code tool.
fn code_tool_lines(text: &mut String, has: impl Fn(&str) -> bool) {
    if has("search") {
        text.push_str(
            "Use search to locate code by concept, identifier, or literal before reading it.\n",
        );
    }
    if has("outline") {
        text.push_str("Use outline to map source structure before reading specific sections.\n");
    }
    if has("zoom") {
        text.push_str(
            "Use zoom to inspect named symbols, with callgraph: true for one-level calls.\n",
        );
    }
    if has("callgraph") {
        text.push_str(
            "Use callgraph for callers, impact, and multi-level execution or data-flow traces.\n",
        );
    }
    if has("inspect") {
        text.push_str("Use inspect after editing for fresh diagnostics; run the project's typecheck and relevant tests as the authoritative gates.\n");
    }
    if has("read") {
        text.push_str(
            "Use read for files and bounded line ranges, not shell commands to explore source.\n",
        );
    }
}

/// The `head` preset's system text: the text AFT served before presets
/// existed, unchanged. `worker` is the legacy `broca` item's own parameter,
/// which adds one line and nothing else.
pub(super) fn broca_text(names: &[&str], worker: bool) -> String {
    let has = |name: &str| names.contains(&name);
    let mut text = String::from("# Agent File Tools\n\n");
    code_tool_lines(&mut text, has);
    if has("bash") {
        text.push_str("Use bash with wait: true for long commands. The provider waits for completion or the command deadline; no completion subscriber is required.\n");
    }
    if worker {
        text.push_str(WORKER_SCOPE_LINE);
    }
    text
}

const WORKER_SCOPE_LINE: &str = "As a worker, keep changes within your assigned scope, verify them, and report the result to your parent.\n";

/// The `worker` preset's system text. A delegated worker is never woken once
/// its turn ends, so it is told how to wait on a command and never to end its
/// turn on one. The wait limit is named by its setting with its default,
/// because the live value is the project's to configure.
pub(super) fn worker_text(names: &[&str]) -> String {
    let has = |name: &str| names.contains(&name);
    let mut text = String::from("# Agent File Tools\n\n");
    code_tool_lines(&mut text, has);
    if has("bash") {
        text.push_str("Use bash with wait: true when you need a long command's result before anything else; a blocking wait returns after the worker wait limit (`bash.worker_wait_max_ms`, 30 minutes by default) and reports that the command is still running.\n");
    }
    if has("bash_watch") {
        text.push_str("A command in the background never wakes you: wait on it with bash_watch before you report a result, and watch again while it is still running. Don't poll bash_status or sleep to wait.\n");
    }
    text.push_str(WORKER_SCOPE_LINE);
    text
}

/// The `reader` preset's system text: what the session can and cannot do,
/// then the shared line for each code tool it has.
pub(super) fn reader_text(names: &[&str]) -> String {
    let has = |name: &str| names.contains(&name);
    let mut text = String::from("# Agent File Tools\n\n");
    text.push_str("This session is read-only: it can read and search code, but it has no tool that runs commands or changes, moves or deletes files.\n");
    code_tool_lines(&mut text, has);
    text
}

pub(super) fn catalog(
    body: Value,
    disabled: &[String],
    powershell_available: bool,
) -> Result<Value, ErrorBody> {
    let request: CatalogRequest = serde_json::from_value(body)
        .map_err(|e| errors::invalid_request("arguments", e.to_string()))?;
    let preset = CatalogPreset::parse("preset", request.preset.as_deref())?;
    if let Some((key, _)) = request.params.iter().next() {
        return Err(errors::invalid_request(
            &format!("params.{key}"),
            "unsupported catalog parameter",
        ));
    }
    let mut answer =
        CatalogAnswer::new("", "").with_tools(tools_for(preset, disabled, powershell_available));
    let composition = request.composition.as_ref().map(|composition| {
        composition_digest(&Value::Object(composition.clone())).expect("composition is JSON")
    });
    answer.composition_digest = composition.clone();
    if let Some(item) = request.system_text {
        // The text item names either the legacy `broca` preset or the
        // catalog's own preset. Either way the catalog preset decides the
        // text, so the text always matches the tools served beside it.
        if item.preset != "broca" && item.preset != preset.name() {
            return Err(errors::invalid_request(
                "system_text.preset",
                format!(
                    "AFT serves the {:?} system text beside the {:?} catalog preset, or the legacy \"broca\" text",
                    preset.name(),
                    preset.name()
                ),
            ));
        }
        for (key, value) in &item.params {
            if key != "worker" || !value.is_boolean() || item.preset != "broca" {
                return Err(errors::invalid_request(
                    &format!("system_text.params.{key}"),
                    "unsupported system-text parameter",
                ));
            }
        }
        let worker = item
            .params
            .get("worker")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let names: Vec<_> = answer.tools.iter().map(|tool| tool.name.as_str()).collect();
        let text = match preset {
            CatalogPreset::Head => broca_text(&names, worker),
            CatalogPreset::Worker => worker_text(&names),
            CatalogPreset::Reader => reader_text(&names),
        };
        // The item digest is the SHA-256 hex of the exact UTF-8 text returned,
        // so a runner can check the text against it from this reply alone. The
        // protocol crate owns that definition; the preflight digest reuses it
        // because the text is the only input that changes the rendered item.
        let digest = system_text_digest(&text);
        let mut rendered = SystemTextAnswer::new(&digest, &digest)
            .with_text(text)
            // The text is composed for exactly the tools served in this reply;
            // naming them lets a runner refuse the text beside another tool set.
            .with_tool_names(names.iter().copied());
        rendered.composition_digest = composition;
        answer.system_text = Some(rendered);
    }
    // Exclude generation and catalog_digest to avoid hashing the digest itself.
    // The commons helper canonicalizes the remaining JSON before hashing it.
    let mut content = serde_json::to_value(&answer).expect("catalog is JSON");
    content.as_object_mut().unwrap().remove("generation");
    content.as_object_mut().unwrap().remove("catalog_digest");
    // The preset is part of the fingerprint, so two presets can never share a
    // generation even if their content one day coincides. `head` adds
    // nothing: an absent preset is `head`, and its fingerprint stays the one
    // runners froze before presets existed.
    if preset != CatalogPreset::Head {
        content
            .as_object_mut()
            .unwrap()
            .insert("preset".into(), json!(preset.name()));
    }
    let digest = composition_digest(&content).expect("catalog has canonical bytes");
    answer.generation = digest.clone();
    answer.catalog_digest = digest.clone();
    if request.digest_only == Some(true) {
        return Ok(json!({"generation": digest, "catalog_digest": digest}));
    }
    Ok(serde_json::to_value(answer).expect("catalog is JSON"))
}

/// Admit a v1 `tool.call` before anything runs, and resolve the role it runs
/// under. The refusals follow the contract's order: the call's own shape,
/// then whether the tool is served, disabled or unavailable, and only then
/// what this route may do with it.
///
/// A plain call needs no scope stamp: the contract runs plain calls on an
/// unscoped route, and the safety boundary is the bind's trust class (shell
/// tools need an admitted principal) together with the bound session.
///
/// The role comes from the call's preset ([`resolve_caller_role`]). A call
/// that names the `reader` preset is refused any tool outside that preset, by
/// name; every other role may call any tool the catalog serves.
pub(super) fn admit(
    call: &cortexkit_role_tool_provider::call::ToolCallRequest,
    scoped_route: bool,
    disabled: &[String],
    powershell_available: bool,
    session: &str,
    trusted: bool,
) -> Result<CallerRole, ErrorBody> {
    check_call(call)?;
    let role = resolve_caller_role(call.preset.as_deref(), scoped_route, false)?;
    admit_as(call, disabled, powershell_available, session, trusted, role)?;
    Ok(role)
}

const SCOPED_PRESET_REFUSAL_WINDOW: Duration = Duration::from_secs(60);
const SCOPED_PRESET_REFUSAL_STATE_LIMIT: usize = 256;

#[derive(Default)]
struct RefusalLogState {
    last_logged_at: Option<Instant>,
    suppressed: u64,
}

static SCOPED_PRESET_REFUSALS: LazyLock<Mutex<HashMap<(String, String), RefusalLogState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
static SCOPED_PRESET_REFUSAL_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Admit a provider call with the route identity available for refusal diagnostics.
pub(super) fn admit_on_route(
    call: &cortexkit_role_tool_provider::call::ToolCallRequest,
    scoped_route: bool,
    disabled: &[String],
    powershell_available: bool,
    session: &str,
    trusted: bool,
    root: &Path,
    channel: u16,
) -> Result<CallerRole, ErrorBody> {
    admit_on_route_at(
        call,
        scoped_route,
        disabled,
        powershell_available,
        session,
        trusted,
        root,
        channel,
        Instant::now(),
    )
}

fn admit_on_route_at(
    call: &cortexkit_role_tool_provider::call::ToolCallRequest,
    scoped_route: bool,
    disabled: &[String],
    powershell_available: bool,
    session: &str,
    trusted: bool,
    root: &Path,
    channel: u16,
    now: Instant,
) -> Result<CallerRole, ErrorBody> {
    let result = admit(
        call,
        scoped_route,
        disabled,
        powershell_available,
        session,
        trusted,
    );
    if scoped_route
        && call.preset.is_none()
        && result
            .as_ref()
            .err()
            .is_some_and(|error| errors::invalid_request_field(error) == Some("preset"))
    {
        log_scoped_preset_refusal_at(session, root, channel, &call.name, now);
    }
    result
}

/// Log a scoped route's missing-preset refusal, keeping repeated broken calls quiet.
pub(super) fn log_scoped_preset_refusal(session: &str, root: &Path, channel: u16, tool: &str) {
    log_scoped_preset_refusal_at(session, root, channel, tool, Instant::now());
}

fn log_scoped_preset_refusal_at(
    session: &str,
    root: &Path,
    channel: u16,
    tool: &str,
    now: Instant,
) {
    let root = root.display().to_string();
    let key = (session.to_string(), root.clone());
    let (should_log, suppressed) = {
        let Ok(mut refusals) = SCOPED_PRESET_REFUSALS.lock() else {
            return;
        };
        if !refusals.contains_key(&key) && refusals.len() >= SCOPED_PRESET_REFUSAL_STATE_LIMIT {
            refusals.retain(|_, state| {
                state.last_logged_at.is_some_and(|last| {
                    now.checked_duration_since(last)
                        .is_some_and(|elapsed| elapsed < SCOPED_PRESET_REFUSAL_WINDOW)
                })
            });
            while refusals.len() >= SCOPED_PRESET_REFUSAL_STATE_LIMIT {
                let oldest = refusals
                    .iter()
                    .min_by_key(|(_, state)| state.last_logged_at)
                    .map(|(key, _)| key.clone());
                let Some(oldest) = oldest else {
                    break;
                };
                refusals.remove(&oldest);
            }
        }
        let state = refusals.entry(key).or_default();
        let elapsed = state
            .last_logged_at
            .and_then(|last| now.checked_duration_since(last))
            .unwrap_or_default();
        if state.last_logged_at.is_some() && elapsed < SCOPED_PRESET_REFUSAL_WINDOW {
            state.suppressed = state.suppressed.saturating_add(1);
            (false, 0)
        } else {
            let suppressed = std::mem::take(&mut state.suppressed);
            state.last_logged_at = Some(now);
            (true, suppressed)
        }
    };
    if !should_log {
        return;
    }
    if suppressed > 0 {
        let line =
            format!("tool call refusals suppressed={suppressed} session={session} root={root}");
        crate::slog_warn!("{line}");
    }
    let line = format!(
        "tool call refused: scoped route without preset tool={tool} session={session} root={root} channel={channel}"
    );
    crate::slog_warn!("{line}");
}

/// [`admit`] for a call whose role is already resolved.
fn admit_as(
    call: &cortexkit_role_tool_provider::call::ToolCallRequest,
    disabled: &[String],
    powershell_available: bool,
    session: &str,
    trusted: bool,
    role: CallerRole,
) -> Result<(), ErrorBody> {
    if !METADATA
        .iter()
        .chain(PRESET_ONLY_METADATA)
        .any(|(name, _, _)| *name == call.name)
    {
        return Err(errors::unknown_tool(&call.name));
    }
    if role == CallerRole::Reader && !READER_TOOLS.contains(&call.name.as_str()) {
        return Err(ErrorBody::new(
            errors::UNKNOWN_TOOL,
            format!(
                "tool {:?} is not served by the \"reader\" preset",
                call.name
            ),
        )
        .with_detail(json!({"tool": call.name, "preset": CatalogPreset::Reader.name()})));
    }
    if crate::tool_gate::refusal_by_name("v1-admission", &call.name, disabled).is_some() {
        return Err(errors::tool_disabled(&call.name));
    }
    if call.name == "powershell" && !powershell_available {
        return Err(
            ErrorBody::new(errors::TOOL_UNAVAILABLE, "PowerShell is not available")
                .with_detail(json!({"tool": call.name, "reason": "under_review"})),
        );
    }
    if session.is_empty() {
        return Err(errors::invalid_request(
            "session",
            "v1 execution requires a bound session",
        ));
    }
    if !trusted
        && matches!(
            call.name.as_str(),
            "bash" | "powershell" | "bash_status" | "bash_kill" | "bash_write" | "bash_watch"
        )
    {
        return Err(ErrorBody::new(
            errors::CAPABILITY_NOT_ADMITTED,
            "shell execution and observation require an admitted principal",
        ));
    }
    // The worker preset serves every head tool plus the preset-only ones, so
    // its catalog holds the served schema of every admitted name.
    let served = served_tools(CatalogPreset::Worker)
        .iter()
        .find(|tool| tool.catalog.name == call.name)
        .expect("admitted tool has a schema");
    let tool = &served.catalog;
    if let Some(encoded) = &call.schema_pin {
        let pin = SchemaPin::parse(encoded)
            .map_err(|error| errors::invalid_request("schema_pin", error.to_string()))?;
        if pin.schema_digest != tool.schema_digest {
            return Err(ErrorBody::new(errors::TOOL_SCHEMA_CHANGED, "schema pin is stale").with_detail(json!({"tool": call.name, "expected": pin.schema_digest, "current": tool.schema_digest})));
        }
        if pin.semantics != tool.semantics {
            return Err(ErrorBody::new(errors::TOOL_SEMANTICS_CHANGED, "semantics pin is stale").with_detail(json!({"tool": call.name, "expected": pin.semantics, "current": tool.semantics})));
        }
    }
    let arguments = call
        .arguments
        .as_object()
        .ok_or_else(|| errors::invalid_request("arguments", "arguments must be an object"))?;
    let properties = tool
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("embedded schemas have properties");
    for key in arguments.keys() {
        if !properties.contains_key(key) {
            return Err(errors::invalid_request(
                key,
                "argument is absent from the served schema",
            ));
        }
        if properties[key].get("x-ck-audience").and_then(Value::as_str) == Some("host") {
            return Err(errors::invalid_request(key, "host-only argument"));
        }
    }
    let validator = served.validator.get_or_init(|| {
        #[cfg(test)]
        ADMISSION_WORK.with(|count| {
            let (catalogs, validators) = count.get();
            count.set((catalogs, validators + 1));
        });
        jsonschema::validator_for(&tool.input_schema)
            .expect("embedded schemas are valid JSON schemas")
    });
    if let Some(error) = validator.iter_errors(&call.arguments).next() {
        let field = error.instance_path.to_string();
        return Err(errors::invalid_request(
            if field.is_empty() {
                "arguments"
            } else {
                field.trim_start_matches('/')
            },
            error.to_string(),
        ));
    }
    Ok(())
}

/// The role a tool call runs under. It decides everything the core words or
/// bounds differently for a delegated worker: the worker wait cap on a
/// blocking bash call, the hand-back and promotion wording, the repeat
/// breaker's advice and the kill-deadline sentence (all of which read
/// [`CallerRole::is_worker`]), and, for `Reader`, which tools a call may use.
///
/// Without a preset, a legacy route takes it from the `worker_session` flag the
/// AFT plugins set beside the call. With one, every route takes
/// it from the catalog preset the call names ([`resolve_caller_role`]), the same
/// preset the runner fetched its catalog with. Neither the scope's kind nor
/// the call's arguments ever decide it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CallerRole {
    #[default]
    Head,
    Worker,
    Reader,
}

impl CallerRole {
    pub(crate) fn from_preset(preset: CatalogPreset) -> Self {
        match preset {
            CatalogPreset::Head => Self::Head,
            CatalogPreset::Worker => Self::Worker,
            CatalogPreset::Reader => Self::Reader,
        }
    }

    /// Whether the core treats the caller as a delegated worker.
    pub(crate) fn is_worker(self) -> bool {
        self == Self::Worker
    }

    /// Whether the tools this caller has include `bash_watch`. A worker
    /// preset includes it; a presetless legacy plugin worker has its own watch
    /// tool, while a presetless v1 call is resolved as `Head`.
    pub(crate) fn has_bash_watch(self) -> bool {
        self == Self::Worker
    }
}

/// What a tool call that names no catalog preset gets on a route without a
/// daemon scope stamp.
///
/// SUBC's rule is that a provider chooses this explicitly, and never the most
/// capable preset. Every caller today sends no preset on an unscoped route:
/// the OpenCode and Pi plugins, Broca's legacy tool routes, and `direct`
/// callers. `head` keeps every one of them on exactly the behaviour they had
/// before presets existed. A scoped route has no default at all (see
/// [`resolve_caller_role`]).
pub(crate) const UNSCOPED_DEFAULT_PRESET: CatalogPreset = CatalogPreset::Head;

/// The one place a tool call's preset becomes the role it runs under.
///
/// `preset` is the call's `ToolCallRequest.preset` (subc-protocol 0.29), the
/// same preset the runner fetched its catalog with. A known preset applies,
/// and an unknown one is refused by name. A call that names none is refused
/// on a route the daemon stamped with a scope, because a scoped session always
/// has a plan that names a preset. On an unscoped route it gets the AFT
/// plugins' `worker_session` role when the call carries that flag, and
/// [`UNSCOPED_DEFAULT_PRESET`] otherwise.
pub(super) fn resolve_caller_role(
    preset: Option<&str>,
    scoped_route: bool,
    plugin_worker_flag: bool,
) -> Result<CallerRole, ErrorBody> {
    match preset {
        Some(name) => CatalogPreset::parse("preset", Some(name)).map(CallerRole::from_preset),
        None if scoped_route => Err(errors::invalid_request(
            "preset",
            "a tool call on a route bound under a scope must name its catalog preset (\"head\", \"worker\" or \"reader\")",
        )),
        None if plugin_worker_flag => Ok(CallerRole::Worker),
        None => Ok(CallerRole::from_preset(UNSCOPED_DEFAULT_PRESET)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_role_tool_provider::catalog::{check_flat_schema, structural_schema};
    use std::collections::BTreeSet;

    /// [`admit`] for a call on an unscoped route that names no preset.
    fn admit_plain(
        call: &cortexkit_role_tool_provider::call::ToolCallRequest,
        disabled: &[String],
        powershell_available: bool,
        session: &str,
        trusted: bool,
    ) -> Result<CallerRole, ErrorBody> {
        admit(
            call,
            false,
            disabled,
            powershell_available,
            session,
            trusted,
        )
    }

    fn call(name: &str, arguments: Value) -> cortexkit_role_tool_provider::call::ToolCallRequest {
        serde_json::from_value(json!({"name": name, "arguments": arguments})).unwrap()
    }

    #[test]
    fn warm_admission_does_not_rebuild_catalog_or_validator() {
        let request = call("read", json!({"filePath": "src/main.rs", "limit": 200}));
        admit(&request, false, &[], true, "session", true).unwrap();
        ADMISSION_WORK.with(|count| count.set((0, 0)));
        for _ in 0..12 {
            admit(&request, false, &[], true, "session", true).unwrap();
        }
        let work = ADMISSION_WORK.with(std::cell::Cell::get);
        assert_eq!(
            work,
            (0, 0),
            "catalog builds, validator compilations: {work:?}"
        );
    }

    #[test]
    fn admission_requires_session() {
        let error = admit_plain(&call("status", json!({})), &[], true, "", true).unwrap_err();
        assert_eq!(error.code, errors::INVALID_REQUEST);
        assert_eq!(error.detail.unwrap()["field"], "session");
    }

    #[test]
    fn admission_refuses_arguments_outside_served_schema() {
        let error = admit_plain(
            &call("status", json!({"not_served": true})),
            &[],
            true,
            "session",
            true,
        )
        .unwrap_err();
        assert_eq!(error.code, errors::INVALID_REQUEST);
        assert_eq!(error.detail.unwrap()["field"], "not_served");
    }

    #[test]
    fn admission_disables_companions_regardless_of_task_spelling() {
        for name in ["bash_status", "bash_kill", "bash_write"] {
            for arguments in [
                json!({"task_id":"x"}),
                json!({"taskId":"x"}),
                json!({"task_id":"x", "taskId":"x"}),
            ] {
                let error = admit_plain(
                    &call(name, arguments),
                    &[name.into()],
                    true,
                    "session",
                    true,
                )
                .unwrap_err();
                assert_eq!(error.code, errors::TOOL_DISABLED, "{name}");
            }
        }
    }

    #[test]
    fn admission_refuses_malformed_schema_pin() {
        let mut request = call("status", json!({}));
        request.schema_pin = Some("malformed".into());
        let error = admit_plain(&request, &[], true, "session", true).unwrap_err();
        assert_eq!(error.code, errors::INVALID_REQUEST);
        assert_eq!(error.detail.unwrap()["field"], "schema_pin");
    }

    #[test]
    fn slice_a_declaration_matches_independent_fixture_without_normalizing_other_fields() {
        let fixtures: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/tool_provider_conformance.json"
        ))
        .unwrap();
        let mut actual = describe();
        assert_eq!(actual["implementation_version"], env!("CARGO_PKG_VERSION"));
        actual["implementation_version"] = json!("<built-package-version>");
        assert_eq!(actual, fixtures["slice_a"]["declaration"]);
    }

    #[test]
    fn every_served_capability_tag_is_defined_or_aft_namespaced() {
        // The commons checker accepts the role's defined unprefixed tags and
        // syntactically valid `<namespace>:<name>/v<N>` tags; AFT's own tags
        // must sit in the `aft` namespace.
        for (name, tag, _) in METADATA {
            if tag.is_empty() {
                continue;
            }
            cortexkit_role_tool_provider::check_capability_tag(tag)
                .unwrap_or_else(|problem| panic!("{name} carries {tag:?}: {problem}"));
            if tag.contains(':') {
                assert!(tag.starts_with("aft:"), "{name} carries foreign {tag:?}");
            } else {
                assert!(
                    cortexkit_role_tool_provider::DEFINED_CAPABILITY_TAGS.contains(tag),
                    "{name} carries undefined {tag:?}"
                );
            }
        }
    }

    #[test]
    fn system_text_names_exactly_the_tools_served_beside_it() {
        for (disabled, available) in [
            (vec![], false),
            (vec![], true),
            (vec!["aft_outline".to_string()], true),
        ] {
            let reply = catalog(
                json!({"system_text": {"preset": "broca", "params": {"worker": true}}}),
                &disabled,
                available,
            )
            .unwrap();
            let mut served: Vec<String> = reply["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_string())
                .collect();
            served.sort();
            served.dedup();
            let named: Vec<String> =
                serde_json::from_value(reply["system_text"]["tool_names"].clone()).unwrap();
            assert_eq!(named, served, "disabled={disabled:?} pwsh={available}");
        }
        // A reply without system text carries no tool names.
        let reply = catalog(json!({}), &[], true).unwrap();
        assert!(reply.get("system_text").is_none());
    }

    #[test]
    fn no_legacy_tool_name_shadows_a_role_operation() {
        // Legacy routes answer role.describe and tool.catalog and refuse the
        // other role ops before the plugin grammar runs, so a tool spelled
        // like a role op would become unreachable on legacy routes.
        let mut names: BTreeSet<String> = BTreeSet::new();
        for available in [false, true] {
            names.extend(tools(&[], available).into_iter().map(|tool| tool.name));
        }
        names.extend(METADATA.iter().map(|(name, _, _)| name.to_string()));
        names.extend(crate::feature_config::CANONICAL_TOOLS.map(String::from));
        for (alias, canonical) in crate::feature_config::LEGACY_TOOL_ALIASES {
            names.insert(alias.to_string());
            names.insert(canonical.to_string());
        }
        let prefixed: Vec<String> = names.iter().map(|name| format!("aft_{name}")).collect();
        names.extend(prefixed);
        for op in [
            cortexkit_role_tool_provider::ops::ROLE_DESCRIBE,
            cortexkit_role_tool_provider::ops::TOOL_CATALOG,
            cortexkit_role_tool_provider::ops::TOOL_WITHDRAW,
            cortexkit_role_tool_provider::ops::LATE_RESULTS,
            cortexkit_role_tool_provider::ops::LATE_RESULTS_ACK,
            "tool.call",
        ] {
            assert!(recognized_operation(op), "{op} is a role operation");
        }
        for name in &names {
            assert!(
                !recognized_operation(name),
                "legacy tool name {name:?} is also a role operation"
            );
        }
    }

    #[test]
    fn bind_role_versions_are_explicit_and_unknown_versions_fail_closed() {
        assert_eq!(route_role(None).unwrap(), RouteRole::Legacy);
        assert_eq!(
            route_role(Some(&BTreeMap::from([("other-role".into(), "v2".into())]))).unwrap(),
            RouteRole::Legacy
        );
        for version in ["v1", "v2", "1", ""] {
            let versions = BTreeMap::from([("tool-provider".into(), version.into())]);
            let result = route_role(Some(&versions));
            if version == "v1" {
                assert_eq!(result.unwrap(), RouteRole::ToolProviderV1);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.code, "unsupported_role_version");
                assert_eq!(error.detail.unwrap()["versions"], json!(["v1"]));
            }
        }
        assert!(
            serde_json::from_value::<BTreeMap<String, String>>(json!({"tool-provider": 1}))
                .is_err()
        );
    }

    #[test]
    fn metadata_is_a_bijection_with_host_manifest_and_disable_keys() {
        let expected: BTreeSet<_> = METADATA.iter().map(|(name, _, _)| *name).collect();
        assert_eq!(expected.len(), 24);
        for available in [false, true] {
            let inventory = tools(&[], available);
            assert_eq!(inventory.len(), if available { 24 } else { 23 });
            for tool in &inventory {
                assert!(expected.contains(tool.name.as_str()));
                assert_eq!(tool.semantics, 1);
                assert_eq!(tool.capabilities.is_empty(), tool.name == "status");
                assert_eq!(
                    tool.result_ops,
                    matches!(tool.name.as_str(), "read" | "bash")
                        .then(|| vec!["prepend".into(), "append".into()])
                );
                assert!(tool.description.is_some());
                assert!(tool.input_schema.get("description").is_none());
                assert!(check_flat_schema(&tool.input_schema).is_ok());
                assert_eq!(
                    schema_digest(&tool.input_schema).unwrap(),
                    tool.schema_digest
                );
                if let Some(key) = crate::tool_gate::canonical_tool_name(&tool.name) {
                    let filtered = tools(&[key.into()], available);
                    assert!(!filtered.iter().any(|entry| entry.name == tool.name));
                    assert_eq!(filtered.len(), inventory.len() - 1);
                } else {
                    assert_eq!(tool.name, "status");
                }
            }
            if available {
                assert_eq!(
                    inventory
                        .iter()
                        .map(|tool| tool.name.as_str())
                        .collect::<BTreeSet<_>>(),
                    expected
                );
            }
        }
    }

    const REGENERATE_CATALOG: &str = "cargo test -p agent-file-tools --lib regenerate_tool_provider_catalog --locked -- --ignored";

    /// The catalog goldens: the `head` file AFT pinned before presets existed,
    /// and one file holding a fixture per non-head preset. Both are rewritten by
    /// the one helper and checked by the one golden test.
    const CATALOG_FIXTURES: &[(&str, &str)] = &[
        (
            "tests/fixtures/tool_provider_catalog.json",
            include_str!("../../tests/fixtures/tool_provider_catalog.json"),
        ),
        (
            "tests/fixtures/tool_provider_catalog_presets.json",
            include_str!("../../tests/fixtures/tool_provider_catalog_presets.json"),
        ),
    ];

    #[test]
    #[ignore = "explicit fixture regeneration, not a verification gate"]
    fn regenerate_tool_provider_catalog() {
        for (relative, _) in CATALOG_FIXTURES {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
            let mut fixtures: Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            for fixture in fixtures.as_array_mut().unwrap() {
                let disabled: Vec<String> =
                    serde_json::from_value(fixture["disabled_tools"].clone()).unwrap();
                fixture["reply"] = catalog(
                    fixture["request"].clone(),
                    &disabled,
                    fixture["powershell_available"].as_bool().unwrap(),
                )
                .unwrap();
            }
            std::fs::write(
                path,
                format!("{}\n", serde_json::to_string_pretty(&fixtures).unwrap()),
            )
            .unwrap();
        }
    }

    #[test]
    fn full_catalog_goldens_and_digest_only_have_exact_identity() {
        let mut presets_seen = BTreeSet::new();
        for (_, raw) in CATALOG_FIXTURES {
            let fixtures: Value = serde_json::from_str(raw).unwrap();
            for fixture in fixtures.as_array().unwrap() {
                presets_seen.insert(
                    fixture["request"]["preset"]
                        .as_str()
                        .unwrap_or("head")
                        .to_string(),
                );
                check_catalog_golden(fixture);
            }
        }
        let every: BTreeSet<String> = CatalogPreset::ALL
            .iter()
            .map(|preset| preset.name().to_string())
            .collect();
        assert_eq!(presets_seen, every, "every preset has a golden");
    }

    fn check_catalog_golden(fixture: &Value) {
        let disabled: Vec<String> =
            serde_json::from_value(fixture["disabled_tools"].clone()).unwrap();
        let available = fixture["powershell_available"].as_bool().unwrap();
        let request = fixture["request"].clone();
        let actual = catalog(request.clone(), &disabled, available).unwrap();
        assert!(
            serde_json::to_vec(&actual).unwrap() == serde_json::to_vec(&fixture["reply"]).unwrap(),
            "full catalog bytes differ for {}; regenerate with: {}",
            fixture["name"],
            REGENERATE_CATALOG
        );
        assert_eq!(actual["generation"], actual["catalog_digest"]);
        let mut digest_request = request;
        digest_request["digest_only"] = json!(true);
        assert_eq!(
            catalog(digest_request, &disabled, available).unwrap(),
            json!({"generation": actual["generation"], "catalog_digest": actual["catalog_digest"]})
        );
        assert_eq!(
            catalog(fixture["request"].clone(), &disabled, available).unwrap(),
            actual
        );
    }

    #[test]
    fn broca_literals_and_preset_refusals() {
        let fixtures: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/tool_provider_system_text.json"
        ))
        .unwrap();
        for (disabled, worker, fixture) in [
            (vec![], false, "all_tools_worker_false"),
            (vec![], true, "all_tools_worker_true"),
            (vec!["bash".into()], false, "bash_disabled_worker_false"),
        ] {
            let answer = catalog(
                json!({"system_text": {"preset": "broca", "params": {"worker": worker}}}),
                &disabled,
                true,
            )
            .unwrap();
            assert_eq!(answer["system_text"]["text"], fixtures[fixture]);
            let text = answer["system_text"]["text"].as_str().unwrap();
            // A runner checks the text against `item_digest` from this reply
            // alone, as the SHA-256 hex of the exact text bytes.
            {
                use sha2::{Digest, Sha256};
                assert_eq!(
                    answer["system_text"]["item_digest"].as_str().unwrap(),
                    format!("{:x}", Sha256::digest(text.as_bytes()))
                );
            }
            // The preflight digest and the protocol crate's helper agree with
            // the independently computed item digest.
            assert_eq!(
                answer["system_text"]["preflight_digest"],
                answer["system_text"]["item_digest"]
            );
            assert_eq!(
                answer["system_text"]["item_digest"].as_str().unwrap(),
                system_text_digest(text)
            );
            assert!(!text.contains("bash_watch"));
            assert!(!text.contains("Long-running-commands"));
            assert_eq!(text.contains("wait: true"), disabled.is_empty());
        }
        for preset in ["opencode", "pi", "unknown", ""] {
            assert_eq!(
                errors::invalid_request_field(
                    &catalog(json!({"system_text": {"preset": preset}}), &[], true).unwrap_err()
                ),
                Some("system_text.preset")
            );
        }
    }

    fn names(reply: &Value) -> Vec<String> {
        reply["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect()
    }

    fn description<'a>(reply: &'a Value, tool: &str) -> &'a str {
        reply["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == tool)
            .and_then(|entry| entry["description"].as_str())
            .unwrap_or_else(|| panic!("{tool} is not served"))
    }

    /// Wording that promises a later wake-up or tells the session to end its
    /// turn: right for a primary, false for a worker that nothing wakes.
    const HEAD_ONLY_WORDING: &[&str] = &[
        "completion reminder",
        "end the turn",
        "end your turn",
        "remind you",
    ];

    fn preset_request(preset: &str) -> Value {
        json!({"preset": preset, "system_text": {"preset": preset}})
    }

    #[test]
    fn head_preset_is_byte_identical_to_the_catalog_without_a_preset() {
        // An absent preset is `head`, and naming `head` must not move a byte
        // or the fingerprint a runner froze before presets existed.
        for available in [false, true] {
            for (worker, text) in [(false, "broca"), (true, "broca"), (false, "head")] {
                let params = if text == "broca" {
                    json!({"worker": worker})
                } else {
                    json!({})
                };
                let unnamed = catalog(
                    json!({"system_text": {"preset": "broca", "params": {"worker": worker}}}),
                    &[],
                    available,
                )
                .unwrap();
                let named = catalog(
                    json!({"preset": "head", "system_text": {"preset": text, "params": params}}),
                    &[],
                    available,
                )
                .unwrap();
                assert_eq!(
                    serde_json::to_vec(&named).unwrap(),
                    serde_json::to_vec(&unnamed).unwrap(),
                    "pwsh={available} worker={worker} text={text}"
                );
            }
            let bare = catalog(json!({}), &[], available).unwrap();
            assert_eq!(
                catalog(json!({"preset": "head"}), &[], available).unwrap(),
                bare
            );
            assert!(!names(&bare).contains(&"bash_watch".to_string()));
        }
    }

    #[test]
    fn worker_preset_adds_bash_watch_with_worker_wording() {
        for available in [false, true] {
            let head = catalog(preset_request("head"), &[], available).unwrap();
            let worker = catalog(preset_request("worker"), &[], available).unwrap();
            let mut expected = names(&head);
            let at = expected
                .iter()
                .position(|name| name == "bash_status")
                .unwrap()
                + 1;
            expected.insert(at, "bash_watch".into());
            assert_eq!(names(&worker), expected, "pwsh={available}");
            let watch = description(&worker, "bash_watch");
            assert!(watch.contains("never wakes you"), "{watch}");
            assert!(watch.contains("`bash.worker_wait_max_ms`, 30 minutes by default"));
            let mut worded = vec!["bash", "bash_status", "bash_watch"];
            if available {
                worded.push("powershell");
            }
            for tool in worded {
                let text = description(&worker, tool).to_lowercase();
                for phrase in HEAD_ONLY_WORDING {
                    assert!(
                        !text.contains(phrase),
                        "worker {tool} says {phrase:?}: {text}"
                    );
                }
                let entry = worker["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|entry| entry["name"] == tool)
                    .unwrap();
                let nested = entry["input_schema"].to_string().to_lowercase();
                for phrase in HEAD_ONLY_WORDING {
                    assert!(
                        !nested.contains(phrase),
                        "worker {tool} argument says {phrase:?}"
                    );
                }
            }
            assert!(description(&worker, "bash").contains("bash_watch"));
            assert!(description(&head, "bash").contains("completion reminder"));
            // Shared tools keep their pins, so a call built against either
            // preset's catalog is admitted the same way.
            for tool in worker["tools"].as_array().unwrap() {
                if let Some(head_tool) = head["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|entry| entry["name"] == tool["name"])
                {
                    assert_eq!(tool["schema_digest"], head_tool["schema_digest"]);
                }
            }
            let text = worker["system_text"]["text"].as_str().unwrap();
            {
                use sha2::{Digest, Sha256};
                assert_eq!(
                    worker["system_text"]["item_digest"].as_str().unwrap(),
                    format!("{:x}", Sha256::digest(text.as_bytes()))
                );
            }
            // The preflight digest and the protocol crate's helper agree with
            // the independently computed item digest.
            assert_eq!(
                worker["system_text"]["preflight_digest"],
                worker["system_text"]["item_digest"]
            );
            assert_eq!(
                worker["system_text"]["item_digest"].as_str().unwrap(),
                system_text_digest(text)
            );
            assert!(text.contains("bash_watch"), "{text}");
            assert!(text.contains("`bash.worker_wait_max_ms`, 30 minutes by default"));
            for phrase in HEAD_ONLY_WORDING {
                assert!(!text.to_lowercase().contains(phrase), "{phrase:?}: {text}");
            }
            let named: Vec<String> =
                serde_json::from_value(worker["system_text"]["tool_names"].clone()).unwrap();
            let mut served = names(&worker);
            served.sort();
            assert_eq!(named, served);
        }
        // `bash_watch` follows its own disable key and `bash`.
        for disabled in ["bash_watch", "bash"] {
            let reply = catalog(json!({"preset": "worker"}), &[disabled.into()], true).unwrap();
            assert!(
                !names(&reply).contains(&"bash_watch".to_string()),
                "{disabled}"
            );
        }
    }

    #[test]
    fn reader_preset_serves_only_read_tools() {
        for available in [false, true] {
            let reader = catalog(preset_request("reader"), &[], available).unwrap();
            let served: BTreeSet<String> = names(&reader).into_iter().collect();
            let expected: BTreeSet<String> =
                READER_TOOLS.iter().map(|name| name.to_string()).collect();
            assert_eq!(served, expected, "pwsh={available}");
            let text = reader["system_text"]["text"].as_str().unwrap();
            assert!(text.contains("read-only"), "{text}");
            assert!(!text.contains("Use bash"), "{text}");
        }
        let reply = catalog(json!({"preset": "reader"}), &["aft_outline".into()], true).unwrap();
        assert!(!names(&reply).contains(&"outline".to_string()));
    }

    #[test]
    fn catalog_fingerprints_differ_across_presets_and_digest_only_agrees() {
        let mut seen = BTreeSet::new();
        for preset in CatalogPreset::ALL {
            for request in [
                json!({"preset": preset.name()}),
                preset_request(preset.name()),
            ] {
                let full = catalog(request.clone(), &[], true).unwrap();
                assert_eq!(full["generation"], full["catalog_digest"]);
                let mut digest_request = request.clone();
                digest_request["digest_only"] = json!(true);
                assert_eq!(
                    catalog(digest_request, &[], true).unwrap(),
                    json!({"generation": full["generation"], "catalog_digest": full["catalog_digest"]}),
                    "{request}"
                );
                assert!(
                    seen.insert(full["catalog_digest"].as_str().unwrap().to_string()),
                    "{request} shares a fingerprint"
                );
            }
        }
    }

    #[test]
    fn unknown_presets_are_refused_by_name() {
        for preset in [
            "main",
            "Worker",
            "",
            "broca",
            "cortexkit-conformance-undefined-preset",
        ] {
            let error = catalog(json!({"preset": preset}), &[], true).unwrap_err();
            assert_eq!(
                errors::invalid_request_field(&error),
                Some("preset"),
                "{preset}"
            );
            assert!(
                error.message.contains(&format!("{preset:?}")),
                "{}",
                error.message
            );
        }
        // The text item names the catalog's preset or the legacy `broca`.
        for (catalog_preset, text) in [("worker", "reader"), ("reader", "head"), ("head", "worker")]
        {
            let error = catalog(
                json!({"preset": catalog_preset, "system_text": {"preset": text}}),
                &[],
                true,
            )
            .unwrap_err();
            assert_eq!(
                errors::invalid_request_field(&error),
                Some("system_text.preset"),
                "{catalog_preset}/{text}"
            );
        }
        let error = catalog(
            json!({"preset": "worker", "system_text": {"preset": "worker", "params": {"worker": true}}}),
            &[],
            true,
        )
        .unwrap_err();
        assert_eq!(
            errors::invalid_request_field(&error),
            Some("system_text.params.worker")
        );
        let legacy = catalog(
            json!({"preset": "worker", "system_text": {"preset": "broca", "params": {"worker": true}}}),
            &[],
            true,
        )
        .unwrap();
        assert_eq!(
            legacy["system_text"]["text"],
            catalog(preset_request("worker"), &[], true).unwrap()["system_text"]["text"]
        );
    }

    #[test]
    fn call_presets_resolve_by_route_kind() {
        // A named preset applies on every route kind.
        for scoped in [false, true] {
            for preset in CatalogPreset::ALL {
                assert_eq!(
                    resolve_caller_role(Some(preset.name()), scoped, false).unwrap(),
                    CallerRole::from_preset(preset),
                    "scoped={scoped}"
                );
                assert_eq!(
                    CallerRole::from_preset(preset).has_bash_watch(),
                    preset == CatalogPreset::Worker,
                    "scoped={scoped} preset={preset:?}"
                );
                // A named preset outranks the plugins' worker flag.
                assert_eq!(
                    resolve_caller_role(Some(preset.name()), scoped, true).unwrap(),
                    CallerRole::from_preset(preset)
                );
            }
            // An unknown preset is refused by name on both route kinds.
            for unknown in ["main", "Worker", ""] {
                let error = resolve_caller_role(Some(unknown), scoped, false).unwrap_err();
                assert_eq!(errors::invalid_request_field(&error), Some("preset"));
                assert!(
                    error.message.contains(&format!("{unknown:?}")),
                    "{}",
                    error.message
                );
            }
        }
        // No preset on an unscoped route: the named default, or the plugins'
        // worker flag, exactly as before presets existed.
        assert_eq!(UNSCOPED_DEFAULT_PRESET, CatalogPreset::Head);
        assert_eq!(
            resolve_caller_role(None, false, false).unwrap(),
            CallerRole::Head
        );
        assert_eq!(
            resolve_caller_role(None, false, true).unwrap(),
            CallerRole::Worker
        );
        assert!(
            resolve_caller_role(None, false, true)
                .unwrap()
                .has_bash_watch(),
            "legacy plugin workers have their own bash_watch tool"
        );
        // No preset on a scoped route: refused by name, whatever the flag.
        for flag in [false, true] {
            let error = resolve_caller_role(None, true, flag).unwrap_err();
            assert_eq!(error.code, errors::INVALID_REQUEST);
            assert_eq!(errors::invalid_request_field(&error), Some("preset"));
        }
        // Arguments never carry the role: admission refuses them as unserved.
        assert_eq!(
            errors::invalid_request_field(
                &admit_plain(
                    &call("status", json!({"preset": "worker"})),
                    &[],
                    true,
                    "session",
                    true
                )
                .unwrap_err()
            ),
            Some("preset")
        );
        // Through admission, the call's own `preset` member decides.
        let with_preset = |preset: Option<&str>| {
            let mut request = call("status", json!({}));
            request.preset = preset.map(str::to_string);
            request
        };
        assert_eq!(
            admit(&with_preset(None), false, &[], true, "session", true).unwrap(),
            CallerRole::Head
        );
        assert_eq!(
            admit(
                &with_preset(Some("worker")),
                true,
                &[],
                true,
                "session",
                true
            )
            .unwrap(),
            CallerRole::Worker
        );
        assert_eq!(
            errors::invalid_request_field(
                &admit(&with_preset(None), true, &[], true, "session", true).unwrap_err()
            ),
            Some("preset")
        );
        assert_eq!(
            errors::invalid_request_field(
                &admit(
                    &with_preset(Some("main")),
                    false,
                    &[],
                    true,
                    "session",
                    true
                )
                .unwrap_err()
            ),
            Some("preset")
        );
        let error = admit(
            &with_preset(Some("reader")),
            false,
            &[],
            true,
            "session",
            true,
        )
        .unwrap_err();
        assert_eq!(error.code, errors::UNKNOWN_TOOL);
        assert_eq!(error.detail.unwrap()["preset"], "reader");
        for preset in CatalogPreset::ALL {
            let role = CallerRole::from_preset(preset);
            assert_eq!(role.is_worker(), preset == CatalogPreset::Worker);
        }
    }

    #[test]
    fn log_capture_scoped_presetless_call_and_suppression() {
        let _serial = SCOPED_PRESET_REFUSAL_TEST_LOCK.lock().unwrap();
        let call = call("status", json!({}));
        let session = format!("preset-refusal-test-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("preset-refusal-test-{}", std::process::id()));
        let channel = 42;
        let start = Instant::now();
        let (_, lines) = crate::logging::capture_log_lines(|| {
            for second in 0..5 {
                let error = admit_on_route_at(
                    &call,
                    true,
                    &[],
                    true,
                    &session,
                    true,
                    &root,
                    channel,
                    start + Duration::from_secs(second),
                )
                .unwrap_err();
                assert_eq!(errors::invalid_request_field(&error), Some("preset"));
            }
            let error = admit_on_route_at(
                &call,
                true,
                &[],
                true,
                &session,
                true,
                &root,
                channel,
                start + SCOPED_PRESET_REFUSAL_WINDOW,
            )
            .unwrap_err();
            assert_eq!(errors::invalid_request_field(&error), Some("preset"));
        });

        let root = root.display().to_string();
        assert_eq!(lines.len(), 3, "captured warning lines: {lines:?}");
        let refusal = format!(
            "tool call refused: scoped route without preset tool=status session={session} root={root} channel={channel}"
        );
        assert!(lines[0].ends_with(&refusal), "{}", lines[0]);
        assert!(
            lines[1].ends_with(&format!(
                "tool call refusals suppressed=4 session={session} root={root}"
            )),
            "{}",
            lines[1]
        );
        assert!(lines[2].ends_with(&refusal), "{}", lines[2]);
    }

    #[test]
    fn scoped_preset_refusal_state_stays_bounded_for_distinct_sessions() {
        let _serial = SCOPED_PRESET_REFUSAL_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("preset-refusal-cap-{}", std::process::id()));
        let now = Instant::now();
        let (_, lines) = crate::logging::capture_log_lines(|| {
            for index in 0..(SCOPED_PRESET_REFUSAL_STATE_LIMIT + 100) {
                log_scoped_preset_refusal_at(
                    &format!("preset-refusal-cap-{index}"),
                    &root,
                    42,
                    "status",
                    now,
                );
            }
        });
        assert_eq!(lines.len(), SCOPED_PRESET_REFUSAL_STATE_LIMIT + 100);
        let refusals = SCOPED_PRESET_REFUSALS.lock().unwrap();
        assert!(
            refusals.len() <= SCOPED_PRESET_REFUSAL_STATE_LIMIT,
            "refusal log state grew to {} entries",
            refusals.len()
        );
    }

    #[test]
    fn a_reader_call_is_refused_every_tool_outside_its_preset() {
        let admit_reader = |name: &str, arguments: Value| {
            admit_as(
                &call(name, arguments),
                &[],
                true,
                "session",
                true,
                CallerRole::Reader,
            )
        };
        for (name, arguments) in [
            ("bash", json!({"command": "true"})),
            ("write", json!({})),
            ("edit", json!({})),
            ("delete", json!({})),
            ("status", json!({})),
        ] {
            let error = admit_reader(name, arguments).unwrap_err();
            assert_eq!(error.code, errors::UNKNOWN_TOOL, "{name}");
            assert_eq!(error.detail.as_ref().unwrap()["tool"], name);
            assert_eq!(error.detail.as_ref().unwrap()["preset"], "reader");
        }
        assert!(admit_reader("grep", json!({"pattern": "x"})).is_ok());
        // The same tools stay admitted for every other role.
        for role in [CallerRole::Head, CallerRole::Worker] {
            assert!(admit_as(&call("status", json!({})), &[], true, "session", true, role).is_ok());
        }
    }

    /// Every worker-specific behaviour in the core reads
    /// `CallerRole::is_worker`, so driving `CallerRole::Worker` through each
    /// one must change it, and `Head` must leave the primary wording.
    #[test]
    fn bash_caller_role_worker_drives_the_worker_behaviours() {
        use crate::commands::bash_orchestrate as orchestrate;
        for role in [CallerRole::Head, CallerRole::Worker] {
            let worker = role.is_worker();
            assert_eq!(
                orchestrate::worker_wait_cap_ms(worker, true, 1_800_000),
                worker.then_some(1_800_000),
                "{role:?} wait cap"
            );
            let promotion = orchestrate::format_promotion_message(
                "bash-1",
                None,
                15_000,
                worker,
                role.has_bash_watch(),
            );
            assert_eq!(promotion.contains("won't wake you"), worker, "{promotion}");
            assert_eq!(
                promotion.contains("completion reminder"),
                !worker,
                "{promotion}"
            );
            let launch = orchestrate::format_background_launch(
                "bash-1",
                false,
                worker,
                role.has_bash_watch(),
            );
            assert_eq!(launch.contains("completion reminder"), !worker, "{launch}");
            let status_context = crate::subc_format::FormatContext {
                worker_session: worker,
                bash_watch_available: Some(role.has_bash_watch()),
                ..Default::default()
            };
            let status_text = crate::subc_format::format_response_with_context(
                "bash_status",
                &crate::protocol::Response::success(
                    "status",
                    json!({"task_id": "bash-1", "status": "running", "mode": "pipes"}),
                ),
                &status_context,
            );
            assert_eq!(
                status_text.contains("bash_watch"),
                role.has_bash_watch(),
                "{role:?}: {status_text}"
            );
            let deadline = orchestrate::kill_deadline_sentence(
                Some(crate::bash_background::registry::HardKillDeadline {
                    limit_ms: 1_800_000,
                    source: crate::bash_background::registry::HardKillSource::Default,
                }),
                0,
                worker,
            );
            assert_eq!(
                deadline.contains("each wait you make"),
                worker,
                "{deadline}"
            );
            let mut text = String::new();
            crate::response_finalize::append_repeat_breaker_reminder(
                &mut text,
                "session",
                &crate::response_finalize::repeat_breaker::RepeatIntervention {
                    tool: "bash".into(),
                    count: 10,
                    span: std::time::Duration::from_secs(60),
                    outputs_identical: true,
                },
                worker,
                role.has_bash_watch(),
            );
            assert_eq!(text.contains("bash_watch"), worker, "{text}");
            assert_eq!(
                text.contains("end the turn") || text.contains("turn must end"),
                !worker,
                "{text}"
            );
        }
    }

    #[test]
    fn scope_stamps_carrying_flow_id_decode_in_a_route_bind() {
        // `ScopeAttributes` refuses unknown fields, so a reader on an older
        // subc-protocol would refuse a stamp naming a flow. Prefrontal starts
        // sending `flow_id` only once every reader decodes it.
        let stamp: subc_protocol::scope::ScopeStamp = serde_json::from_value(json!({
            "owner": {"kind": "direct"},
            "ref": "scope-ref",
            "scope_epoch": 3,
            "kind": "worker",
            "attributes": {"agent_id": "agent", "delegates": true, "flow_id": "flow-1"},
            "owner_authorized": true,
        }))
        .unwrap();
        assert_eq!(stamp.attributes.flow_id.as_deref(), Some("flow-1"));
        let bind = subc_protocol::session::ModuleControlRequest::RouteBind {
            route_channel: 1,
            epoch: 1,
            target: subc_protocol::RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            identity: subc_protocol::BindIdentity::new("/tmp/project", "runner", "session"),
            principal: Some(subc_protocol::Principal::Direct),
            consumer_capabilities: None,
            admission_facts: Default::default(),
            scope: Some(stamp.clone()),
            role_versions: None,
        };
        let decoded: subc_protocol::session::ModuleControlRequest =
            serde_json::from_slice(&serde_json::to_vec(&bind).unwrap()).unwrap();
        let subc_protocol::session::ModuleControlRequest::RouteBind { scope, .. } = decoded else {
            panic!("expected a route bind");
        };
        assert_eq!(scope, Some(stamp));
    }

    #[test]
    fn presentation_and_nested_description_edits_do_not_move_schema_pins() {
        let mut raw: Value =
            serde_json::from_str(include_str!("../subc_tool_schemas.json")).unwrap();
        let raw_bash_digest = schema_digest(&raw["bash"]).unwrap();
        let served = tools(&[], true)
            .into_iter()
            .find(|tool| tool.name == "bash")
            .unwrap();
        assert_ne!(raw_bash_digest, served.schema_digest);
        let mut changed = served.input_schema.clone();
        changed["properties"]["command"]["description"] = json!("different nested prose");
        assert_eq!(schema_digest(&changed).unwrap(), served.schema_digest);
        assert_eq!(
            structural_schema(&changed),
            structural_schema(&served.input_schema)
        );
        raw["bash"]["description"] = json!("different top-level prose");
        assert_eq!(schema_digest(&raw["bash"]).unwrap(), raw_bash_digest);
        assert!(served.input_schema["properties"]["description"].is_object());
    }

    #[test]
    fn v1_refusal_precedence_and_served_arguments() {
        for name in ["bash_status", "bash_kill", "bash_write"] {
            let disabled = vec![name.into()];
            for arguments in [
                json!({"task_id": "task"}),
                json!({"taskId": "task"}),
                json!({"taskId": "task", "task_id": "task"}),
            ] {
                assert_eq!(
                    admit_plain(&call(name, arguments), &disabled, true, "session", true)
                        .unwrap_err()
                        .code,
                    "tool_disabled"
                );
            }
        }
        for name in [
            "configure",
            "undo_preview",
            "bash_drain_completions",
            "aft_zoom",
            "missing",
        ] {
            assert_eq!(
                admit_plain(&call(name, json!({})), &[], true, "session", true)
                    .unwrap_err()
                    .code,
                "unknown_tool"
            );
        }
        for name in ["bash", "powershell"] {
            assert_eq!(
                errors::invalid_request_field(
                    &admit_plain(
                        &call(name, json!({"command": "echo x"})),
                        &[],
                        true,
                        "",
                        true
                    )
                    .unwrap_err()
                ),
                Some("session")
            );
            // A route without a daemon scope stamp still runs plain calls.
            assert!(admit_plain(
                &call(name, json!({"command": "echo x"})),
                &[],
                true,
                "session",
                true
            )
            .is_ok());
        }
        for key in [
            "foreground_orchestrate",
            "block_to_completion",
            "shell",
            "unknown",
        ] {
            let mut arguments = json!({"command": "echo x"});
            arguments[key] = if key == "shell" {
                json!("powershell")
            } else {
                json!(true)
            };
            assert_eq!(
                errors::invalid_request_field(
                    &admit_plain(&call("bash", arguments), &[], true, "session", true).unwrap_err()
                ),
                Some(key)
            );
        }
        for arguments in [
            json!({}),
            json!({"command": 1}),
            json!({"command": "echo", "timeout": 0}),
            json!({"command": "echo", "ptyRows": 61}),
        ] {
            assert_eq!(
                admit_plain(&call("bash", arguments), &[], true, "session", true)
                    .unwrap_err()
                    .code,
                "invalid_request"
            );
        }
        assert!(admit_plain(
            &call("bash", json!({"command": "echo", "wait": true})),
            &[],
            true,
            "session",
            true
        )
        .is_ok());
        assert_eq!(
            admit_plain(
                &call("powershell", json!({"command": "echo"})),
                &[],
                false,
                "session",
                true
            )
            .unwrap_err()
            .code,
            "tool_unavailable"
        );
        assert_eq!(
            admit_plain(
                &call("bash", json!({"command": "echo", "sandbox": "host"})),
                &[],
                true,
                "session",
                false
            )
            .unwrap_err()
            .code,
            "capability_not_admitted"
        );
        let schema = tools(&[], true)
            .into_iter()
            .find(|tool| tool.name == "bash")
            .unwrap();
        for (digest, semantics, expected) in [
            (schema.schema_digest.as_str(), 1, None),
            (
                "0000000000000000000000000000000000000000000000000000000000000000",
                2,
                Some("tool_schema_changed"),
            ),
            (
                schema.schema_digest.as_str(),
                2,
                Some("tool_semantics_changed"),
            ),
        ] {
            let mut request = call("bash", json!({"command": "echo"}));
            request.schema_pin = Some(SchemaPin::new("bash", digest, semantics).encode().unwrap());
            let result = admit_plain(&request, &[], true, "session", true);
            match expected {
                Some(code) => assert_eq!(result.unwrap_err().code, code),
                None => assert!(result.is_ok()),
            }
        }
    }
}

#[cfg(test)]
mod route_tests {
    use super::super::*;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static ACTIONS: AtomicUsize = AtomicUsize::new(0);
    /// Dispatches whose request carried the delegated-worker caller role.
    static WORKER_ACTIONS: AtomicUsize = AtomicUsize::new(0);
    /// The last dispatched request's command, session and parameters.
    static LAST_DISPATCH: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);
    static EXCHANGE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn exchange(
        body: Value,
        role: RouteRole,
        session: &str,
        disabled: Vec<String>,
    ) -> WriterFrame {
        let scope = serde_json::from_value(json!({"owner": {"kind": "direct"}, "ref": "scope", "scope_epoch": 1, "kind": "head", "owner_authorized": true})).unwrap();
        // A scoped route refuses a tool call that names no preset, so the
        // tool calls these tests send under a head scope name `head`, as a
        // runner that fetched the head catalog would. Role operations and
        // management ops carry no preset.
        let mut body = body;
        let is_tool_call = body
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| !recognized_operation(name));
        if is_tool_call && body.get("preset").is_none() {
            body["preset"] = json!("head");
        }
        exchange_with_scope(body, role, session, disabled, Some(scope)).await
    }

    async fn exchange_with_scope(
        body: Value,
        role: RouteRole,
        session: &str,
        disabled: Vec<String>,
        scope: Option<subc_protocol::scope::ScopeStamp>,
    ) -> WriterFrame {
        exchange_with_metrics(
            body,
            role,
            session,
            disabled,
            scope,
            &Arc::new(DispatchPathMetrics::new()),
        )
        .await
    }

    async fn exchange_with_metrics(
        body: Value,
        role: RouteRole,
        session: &str,
        disabled: Vec<String>,
        scope: Option<subc_protocol::scope::ScopeStamp>,
        metrics: &Arc<DispatchPathMetrics>,
    ) -> WriterFrame {
        let (_dir, root) = test_support::test_root("tool-provider-admission");
        let ctx = test_support::test_ctx();
        ctx.mark_database_runtime_initializing_for_test();
        let identity = RouteIdentity(Arc::new(RouteIdentityData {
            root: root.clone(),
            project_root: root.as_path().into(),
            harness: "runner".into(),
            session: session.into(),
            role,
            trust: BindTrust::FirstParty,
            spawn_principal: AuthenticatedPrincipal::FirstParty,
            consumer_elicitation_capable: false,
            disabled_tools: Arc::new(disabled),
            scope,
            made_tool_call: AtomicBool::new(false),
        }));
        exchange_with_actor(body, identity, ctx, metrics, |request, _| {
            ACTIONS.fetch_add(1, Ordering::SeqCst);
            if request.worker_session() {
                WORKER_ACTIONS.fetch_add(1, Ordering::SeqCst);
            }
            *LAST_DISPATCH.lock().unwrap() = Some(json!({
                "command": request.command,
                "session_id": request.session_id,
                "params": request.params,
            }));
            Response::success(request.id, json!({}))
        })
        .await
    }

    async fn exchange_with_actor(
        body: Value,
        identity: RouteIdentity,
        ctx: Arc<AppContext>,
        metrics: &Arc<DispatchPathMetrics>,
        dispatch: DispatchFn,
    ) -> WriterFrame {
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(identity.root.clone(), ctx));
        let routes = HashMap::from([(route_key(41, 1), identity)]);
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            41,
            1,
            7,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        let (writer, mut replies) = mpsc::channel(8);
        let (bash_tx, _bash_rx) = mpsc::channel(8);
        let (touch_tx, _touch_rx) = mpsc::channel(8);
        let (deferred_tx, _deferred_rx) = mpsc::unbounded_channel();
        let deferred_tx = super::super::DeferredResponseSender {
            entries: deferred_tx,
            wake: crate::response_finalize::DeferredResponseWake::default(),
        };
        handle_tool_call(
            &writer,
            &frame,
            PhaseTrace::new(Instant::now()),
            &routes,
            &HashMap::new(),
            &crate::subc::ReclaimedRoutes::default(),
            &mut HashMap::new(),
            &executor,
            &Arc::default(),
            &Arc::new(AtomicUsize::new(0)),
            &Arc::new(Notify::new()),
            &PersistentCancelSignal::new(),
            &bash_tx,
            &touch_tx,
            metrics,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut 1,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            dispatch,
            &deferred_tx,
            false,
            1024 * 1024,
            &drain::ModuleDrainWindow::default(),
        )
        .await
        .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(3), replies.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            reply.header.ty,
            FrameType::Response | FrameType::Error | FrameType::StreamEnd
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(30), replies.recv())
                .await
                .is_err()
        );
        reply
    }

    #[cfg(unix)]
    fn runner_restore_context(root: &std::path::Path) -> Arc<AppContext> {
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.join("project")),
                storage_dir: Some(root.join("storage")),
                harness: Some(crate::harness::Harness::Runner),
                experimental_bash_background: true,
                ..crate::config::Config::default()
            },
        ));
        ctx.update_config(|config| config.sandbox.enabled = false);
        ctx.bash_background()
            .set_harness(crate::harness::Harness::Runner);
        ctx.bash_background()
            .set_db_pool(Arc::new(std::sync::Mutex::new(
                crate::db::open(&root.join("storage/aft.db")).unwrap(),
            )));
        ctx
    }

    #[cfg(unix)]
    fn runner_restore_principal(project: &std::path::Path) -> AuthenticatedPrincipal {
        AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::FirstParty,
            route_channel: 41,
            route_epoch: 1,
            project_root: project.into(),
            harness: "runner".into(),
            session_id: "runner-restore".into(),
            principal_id: Some("reserved:broca".into()),
        }
    }

    /// Invoked in a separate libtest process by the restore regression. A
    /// normal test run does nothing; only the explicit fixture directory can
    /// create storage or a child, never the operator's storage root.
    #[cfg(unix)]
    #[test]
    fn runner_restore_process_fixture() {
        let Some(root) = std::env::var_os("AFT_TEST_RUNNER_RESTORE_DIR") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let ctx = runner_restore_context(&root);
        let request: crate::protocol::RawRequest = serde_json::from_value(json!({
            "id": "runner-start", "command": "bash", "session_id": "runner-restore",
            "params": { "command": "printf 'stdout before restart\\n'; printf 'stderr before restart\\n' >&2; while [ ! -f release ]; do sleep 0.05; done; printf 'stdout after restart\\n'; printf 'stderr after restart\\n' >&2; while [ ! -f finish ]; do sleep 0.05; done",
                "background": true, "compressed": true },
        })).unwrap();
        let response = crate::sandbox_spawn::with_authenticated_principal(
            runner_restore_principal(&root.join("project")),
            || crate::commands::bash::handle(&request, &ctx),
        );
        assert!(response.success, "{:?}", response.data);
        let task_id = response.data["task_id"].as_str().unwrap();
        let paths = crate::bash_background::persistence::resolve_task(
            &root.join("storage/runner"),
            "runner-restore",
            task_id,
        )
        .unwrap()
        .paths;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !std::fs::read_to_string(&paths.stderr)
            .unwrap()
            .contains("stderr before restart")
        {
            assert!(Instant::now() < deadline, "fixture produced no output");
            std::thread::sleep(Duration::from_millis(10));
        }
        let request: crate::protocol::RawRequest = serde_json::from_value(json!({
            "id": "runner-initial-output", "command": "bash_status", "session_id": "runner-restore",
            "params": { "task_id": task_id, "output_offset": 0, "stderr_offset": 0 },
        }))
        .unwrap();
        let initial = crate::commands::bash_status::handle(&request, &ctx);
        assert!(initial.success, "{:?}", initial.data);
        std::fs::write(
            root.join("initial-status.json"),
            serde_json::to_vec(&initial.data).unwrap(),
        )
        .unwrap();
        std::fs::write(root.join("task-id"), task_id).unwrap();
        ctx.bash_background().detach();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runner_tool_provider_restore_preserves_minted_task_output_across_processes() {
        use crate::bash_background::persistence::{read_exit_marker, read_task, resolve_task};
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "subc::tool_provider::route_tests::runner_restore_process_fixture",
                "--nocapture",
            ])
            .env("AFT_TEST_RUNNER_RESTORE_DIR", &root)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
        let task_id = std::fs::read_to_string(root.join("task-id")).unwrap();
        let initial: Value =
            serde_json::from_slice(&std::fs::read(root.join("initial-status.json")).unwrap())
                .unwrap();
        assert_eq!(initial["status"], "running");
        for stream in ["stdout", "stderr"] {
            assert!(
                initial["output_preview"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("{stream} before restart")),
                "{initial}"
            );
        }
        let mut offsets = [0, 0];
        let task_storage = root.join("storage/runner");
        let task = resolve_task(&task_storage, "runner-restore", &task_id).unwrap();
        let metadata = read_task(&task.paths.json).unwrap();
        let key = metadata.call_key.as_ref().unwrap();
        assert_eq!(key.requester, "reserved:broca");
        assert_eq!(key.key, task_id);
        assert!(key.minted);
        assert!(!metadata.sandbox_native);
        assert!(task.paths.sandbox_unavailable.exists());

        // Ensure cleanup even when an output assertion fails, and never leave
        // a detached fixture command running past this test.
        struct StopChild(i32);
        impl Drop for StopChild {
            fn drop(&mut self) {
                unsafe {
                    libc::killpg(self.0, libc::SIGKILL);
                }
            }
        }
        let _stop = StopChild(metadata.pgid.unwrap());
        let ctx = runner_restore_context(&root);
        let project_id = ProjectRootId::from_path(&project).unwrap();
        let identity = RouteIdentity(Arc::new(RouteIdentityData {
            root: project_id,
            project_root: project.clone(),
            harness: "runner".into(),
            session: "runner-restore".into(),
            role: RouteRole::ToolProviderV1,
            trust: BindTrust::FirstParty,
            spawn_principal: runner_restore_principal(&project),
            consumer_elicitation_capable: false,
            disabled_tools: Arc::default(),
            scope: None,
            made_tool_call: AtomicBool::new(false),
        }));
        // Wait on filesystem evidence, not on the command's completion. The
        // old AFT process has exited while the detached shell keeps writing.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !std::fs::read_to_string(&task.paths.stderr)
            .unwrap()
            .contains("stderr before restart")
        {
            assert!(Instant::now() < deadline, "fixture produced no output");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        for phase in ["before", "after"] {
            if phase == "after" {
                std::fs::write(project.join("release"), "go").unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                while !std::fs::read_to_string(&task.paths.stderr)
                    .unwrap()
                    .contains("stderr after restart")
                {
                    assert!(
                        Instant::now() < deadline,
                        "fixture stopped writing after restart"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            let reply = exchange_with_actor(
                json!({"name": "bash_status", "arguments": {"taskId": task_id}, "preset": "worker"}),
                identity.clone(), ctx.clone(), &Arc::new(DispatchPathMetrics::new()),
                |request, ctx| crate::commands::bash_status::handle(&request, ctx),
            ).await;
            let reply: Value = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(reply["structuredContent"]["status"], "running", "{reply}");
            let output = reply["structuredContent"]["output_preview"]
                .as_str()
                .unwrap();
            for stream in ["stdout", "stderr"] {
                let expected = format!("{stream} {phase} restart");
                assert!(output.contains(&expected), "{reply}");
            }
            // Piped status text deliberately hides running previews to avoid
            // encouraging polling. The structured reply still retains them.
            assert_eq!(
                reply["content"][0]["text"],
                format!("Task {task_id}: running\nTo wait for it, call bash_watch; don't poll.")
            );
            // Range reads are native protocol fields, not tool-provider
            // catalog arguments. Verify that cursors acquired by the old
            // process can resume exactly at the new bytes after adoption.
            let request: crate::protocol::RawRequest = serde_json::from_value(json!({
                "id": "runner-resumed-output", "command": "bash_status", "session_id": "runner-restore",
                "params": { "task_id": task_id, "output_offset": offsets[0], "stderr_offset": offsets[1] },
            })).unwrap();
            let resumed = crate::commands::bash_status::handle(&request, &ctx);
            assert!(resumed.success, "{:?}", resumed.data);
            for (index, (stream, chunk, cursor)) in [
                ("stdout", "output_chunk_base64", "output_next_offset"),
                ("stderr", "stderr_chunk_base64", "stderr_next_offset"),
            ]
            .into_iter()
            .enumerate()
            {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(resumed.data[chunk].as_str().unwrap())
                    .unwrap();
                assert_eq!(bytes, format!("{stream} {phase} restart\n").as_bytes());
                let next = resumed.data[cursor].as_u64().unwrap();
                assert_eq!(next, offsets[index] + bytes.len() as u64);
                if phase == "before" {
                    assert_eq!(resumed.data[chunk], initial[chunk]);
                    assert_eq!(resumed.data[cursor], initial[cursor]);
                }
                offsets[index] = next;
            }
        }
        std::fs::write(project.join("finish"), "go").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while read_exit_marker(&task.paths).unwrap().is_none() {
            assert!(Instant::now() < deadline, "fixture did not complete");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let reply = exchange_with_actor(
            json!({"name": "bash_status", "arguments": {"taskId": task_id}, "preset": "worker"}),
            identity,
            ctx.clone(),
            &Arc::new(DispatchPathMetrics::new()),
            |request, ctx| crate::commands::bash_status::handle(&request, ctx),
        )
        .await;
        let reply: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(reply["structuredContent"]["status"], "completed", "{reply}");
        let text = reply["content"][0]["text"].as_str().unwrap();
        assert!(
            text.starts_with(&format!("Task {task_id}: completed (exit 0)")),
            "{text}"
        );
        for stream in ["stdout", "stderr"] {
            for phase in ["before", "after"] {
                let expected = format!("{stream} {phase} restart");
                assert!(
                    reply["structuredContent"]["output_preview"]
                        .as_str()
                        .unwrap()
                        .contains(&expected),
                    "{reply}"
                );
                assert!(text.contains(&expected), "{text}");
            }
        }
        ctx.bash_background().detach();
    }

    #[test]
    fn v1_standard_edit_grammar_does_not_register_or_change_plugin_hashline_bindings() {
        let (_dir, root) = test_support::test_root("v1-edit-grammar");
        let arguments =
            json!({"filePath": "file.rs", "edits": [{"oldString":"old", "newString":"new"}]});
        for session in ["session", crate::protocol::DEFAULT_SESSION_ID] {
            for before in [false, true] {
                let ctx = test_support::test_ctx();
                ctx.update_config(|config| config.hashline_enabled = true);
                let registration = || {
                    ctx.hashline_bindings().register(
                        root.as_path(),
                        session,
                        crate::hashline::integration::RegistrationRequest {
                            configured_enabled: true,
                            edit_slot_survives: true,
                            read_slot_survives: true,
                        },
                    )
                };
                if before {
                    registration();
                }
                let call_context = ToolCallContext {
                    project_root: root.as_path().into(),
                    session_id: Some(session.into()),
                    request_id: "standard-edit".into(),
                    diagnostics_on_edit: false,
                    preview: false,
                    edit_slot_survives: None,
                    report_registration_downgrade: false,
                    standard_edit_grammar: true,
                    disabled_tools: Some(Arc::default()),
                    worker_session: false,
                };
                let result = prepare_tool_call(
                    "edit",
                    arguments.clone(),
                    &crate::subc_format::FormatContext::default(),
                    &call_context,
                    &ctx,
                    None,
                )
                .unwrap();
                assert_eq!(result.request.command, "batch");
                assert_eq!(result.request.params["edits"][0]["match"], "old");
                assert_eq!(result.request.params["edits"][0]["replacement"], "new");
                registration();
                let capture = ctx.hashline_bindings().capture(root.as_path(), session);
                assert!(crate::hashline::integration::effective_for_capture(
                    capture.as_ref()
                ));
                let mut legacy_context = call_context.clone();
                legacy_context.standard_edit_grammar = false;
                assert!(
                    prepare_tool_call(
                        "edit",
                        arguments.clone(),
                        &crate::subc_format::FormatContext::default(),
                        &legacy_context,
                        &ctx,
                        None
                    )
                    .is_err(),
                    "legacy hashline binding unexpectedly accepts standard edits"
                );
                let capture = ctx.hashline_bindings().capture(root.as_path(), session);
                assert!(crate::hashline::integration::effective_for_capture(
                    capture.as_ref()
                ));
            }
        }
    }

    #[tokio::test]
    async fn catalog_queues_behind_reads_but_not_database_or_cold_builds() {
        let (_dir, root) = test_support::test_root("catalog-pure-read");
        let (_heavy_dir, heavy_root) = test_support::test_root("catalog-cold-build");
        let executor = Arc::new(Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 6,
            read_cap: 1,
            actor_cap: 2,
            heavy_permits: 1,
            drr_quantum: 1,
        }));
        let ctx = test_support::test_ctx();
        ctx.mark_database_runtime_initializing_for_test();
        assert!(executor.register_actor(root.clone(), ctx.clone()));
        assert!(executor.register_actor(heavy_root.clone(), test_support::test_ctx()));
        let (heavy_started, started) = std::sync::mpsc::channel();
        let (release_heavy, heavy_release) = std::sync::mpsc::channel();
        let heavy = executor.submit_async(
            heavy_root,
            Lane::HeavyInit,
            "held-cold-build".into(),
            Box::new(move |_| {
                heavy_started.send(()).unwrap();
                heavy_release.recv().unwrap();
                Response::success("heavy", json!({}))
            }),
        );
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        let (read_started, started) = std::sync::mpsc::channel();
        let (release_read, read_release) = std::sync::mpsc::channel();
        let read = executor.submit_async(
            root.clone(),
            Lane::PureRead,
            "held-read".into(),
            Box::new(move |_| {
                read_started.send(()).unwrap();
                read_release.recv().unwrap();
                Response::success("read", json!({}))
            }),
        );
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        let (writer, mut replies) = mpsc::channel(8);
        let frame = Frame::build(FrameType::Request, control_flags(), 41, 1, 7, vec![]).unwrap();
        submit_provider_read(
            &writer,
            &frame,
            test_support::route_identity(&root, ""),
            &executor,
            &Arc::default(),
            &Arc::new(DispatchPathMetrics::new()),
            "tool.catalog",
            json!({}),
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), replies.recv())
                .await
                .is_err(),
            "catalogs must share existing read permits"
        );
        release_read.send(()).unwrap();
        assert!(read.await.unwrap().success);
        let reply = tokio::time::timeout(Duration::from_secs(3), replies.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.header.ty, FrameType::Response);
        assert!(ctx.database_runtime_pending("write"));
        assert_eq!(executor.heavy_permits(), 1);
        release_heavy.send(()).unwrap();
        assert!(heavy.await.unwrap().success);
    }

    #[tokio::test]
    async fn unscoped_v1_route_runs_plain_calls_and_refuses_unserved_custody_ops() {
        // A route the daemon stamped with no scope may run plain calls, as
        // the role contract's plain stamp expects. Withdraw and late results
        // are not served by AFT on any route yet, so an unscoped request for
        // them is refused as an unsupported operation without dispatching.
        let _guard = EXCHANGE_LOCK.lock().await;
        ACTIONS.store(0, Ordering::SeqCst);
        let reply = exchange_with_scope(
            json!({"name":"status", "arguments":{}}),
            RouteRole::ToolProviderV1,
            "session",
            vec![],
            None,
        )
        .await;
        assert_eq!(reply.header.ty, FrameType::Response);
        assert_eq!(ACTIONS.load(Ordering::SeqCst), 1);
        for body in [
            json!({"name":"tool.withdraw", "arguments":{"call_key":"key"}}),
            json!({"name":"late_results", "arguments":{"since":null}}),
            json!({"name":"late_results.ack", "arguments":{"through":"cursor"}}),
        ] {
            let reply = exchange_with_scope(
                body.clone(),
                RouteRole::ToolProviderV1,
                "session",
                vec![],
                None,
            )
            .await;
            assert_eq!(reply.header.ty, FrameType::Error, "{body}");
            let error: subc_protocol::ErrorBody = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(error.code, "unsupported_operation", "{body}");
        }
        assert_eq!(ACTIONS.load(Ordering::SeqCst), 1);
    }

    fn stamp(kind: &str) -> subc_protocol::scope::ScopeStamp {
        serde_json::from_value(json!({"owner": {"kind": "direct"}, "ref": "scope", "scope_epoch": 1, "kind": kind, "owner_authorized": true})).unwrap()
    }

    fn error_body(reply: &WriterFrame) -> subc_protocol::ErrorBody {
        assert_eq!(reply.header.ty, FrameType::Error);
        serde_json::from_slice(&reply.body).unwrap()
    }

    fn presets_health(metrics: &DispatchPathMetrics) -> Value {
        metrics.snapshot(&HashMap::new())["tool_call_presets"].clone()
    }

    /// Both route kinds, with and without a preset, through the real tool-call
    /// handler: a missing preset gets `head` on an unscoped route and is
    /// refused by name before dispatch on a scoped one, on legacy and v1
    /// routes alike.
    #[tokio::test]
    async fn missing_preset_is_head_unscoped_and_refused_scoped() {
        let _guard = EXCHANGE_LOCK.lock().await;
        for role in [RouteRole::Legacy, RouteRole::ToolProviderV1] {
            for preset in [None, Some("head"), Some("worker")] {
                for scope in [None, Some(stamp("worker")), Some(stamp("head"))] {
                    ACTIONS.store(0, Ordering::SeqCst);
                    let scoped = scope.is_some();
                    let mut body = json!({"name": "status", "arguments": {}});
                    if let Some(preset) = preset {
                        body["preset"] = json!(preset);
                    }
                    let metrics = Arc::new(DispatchPathMetrics::new());
                    let reply =
                        exchange_with_metrics(body, role, "session", vec![], scope, &metrics).await;
                    let label = format!("{role:?} preset={preset:?} scoped={scoped}");
                    if scoped && preset.is_none() {
                        let error = error_body(&reply);
                        assert_eq!(error.code, "invalid_request", "{label}");
                        assert_eq!(error.detail.unwrap()["field"], "preset", "{label}");
                        assert_eq!(ACTIONS.load(Ordering::SeqCst), 0, "{label} dispatched");
                    } else {
                        assert_eq!(reply.header.ty, FrameType::Response, "{label}");
                        assert_eq!(ACTIONS.load(Ordering::SeqCst), 1, "{label}");
                    }
                    let health = presets_health(&metrics);
                    let presetless = u64::from(!scoped && preset.is_none());
                    assert_eq!(
                        health["tool_calls_without_preset"]["total"], presetless,
                        "{label}: {health}"
                    );
                    if presetless == 1 {
                        assert_eq!(
                            health["tool_calls_without_preset"]["by_harness"]["runner"],
                            1
                        );
                    }
                    assert_eq!(
                        health["scoped_routes_with_tool_calls"],
                        u64::from(scoped),
                        "{label}: {health}"
                    );
                }
            }
        }
    }

    /// The AFT plugins name `head` or `worker` on every call (the bridge's
    /// `callPresetFor`), so a plugin route that the daemon stamped with a
    /// scope keeps serving them; only a call that names no preset is refused.
    #[tokio::test]
    async fn scoped_plugin_style_calls_naming_a_preset_are_served() {
        let _guard = EXCHANGE_LOCK.lock().await;
        for (body, worker) in [
            (
                json!({"name": "status", "arguments": {}, "preset": "head"}),
                false,
            ),
            (
                json!({"name": "status", "arguments": {}, "preset": "worker", "worker_session": true}),
                true,
            ),
        ] {
            ACTIONS.store(0, Ordering::SeqCst);
            WORKER_ACTIONS.store(0, Ordering::SeqCst);
            let reply = exchange_with_scope(
                body.clone(),
                RouteRole::Legacy,
                "session",
                vec![],
                Some(stamp("head")),
            )
            .await;
            assert_eq!(reply.header.ty, FrameType::Response, "{body}");
            assert_eq!(ACTIONS.load(Ordering::SeqCst), 1, "{body}");
            assert_eq!(
                WORKER_ACTIONS.load(Ordering::SeqCst),
                usize::from(worker),
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn call_presets_refuse_unknown_names_and_reader_tools_on_every_route() {
        let _guard = EXCHANGE_LOCK.lock().await;
        for role in [RouteRole::Legacy, RouteRole::ToolProviderV1] {
            for scope in [None, Some(stamp("worker"))] {
                ACTIONS.store(0, Ordering::SeqCst);
                let label = format!("{role:?} scoped={}", scope.is_some());
                let unknown = error_body(
                    &exchange_with_scope(
                        json!({"name": "status", "arguments": {}, "preset": "main"}),
                        role,
                        "session",
                        vec![],
                        scope.clone(),
                    )
                    .await,
                );
                assert_eq!(unknown.code, "invalid_request", "{label}");
                assert_eq!(unknown.detail.unwrap()["field"], "preset", "{label}");
                assert!(
                    unknown.message.contains("\"main\""),
                    "{label}: {}",
                    unknown.message
                );
                let refused = error_body(
                    &exchange_with_scope(
                        json!({"name": "status", "arguments": {}, "preset": "reader"}),
                        role,
                        "session",
                        vec![],
                        scope.clone(),
                    )
                    .await,
                );
                assert_eq!(refused.code, "unknown_tool", "{label}");
                let detail = refused.detail.unwrap();
                assert_eq!(detail["tool"], "status", "{label}");
                assert_eq!(detail["preset"], "reader", "{label}");
                assert_eq!(ACTIONS.load(Ordering::SeqCst), 0, "{label} dispatched");
                let reply = exchange_with_scope(
                    json!({"name": "glob", "arguments": {"pattern": "*.none"}, "preset": "reader"}),
                    role,
                    "session",
                    vec![],
                    scope,
                )
                .await;
                assert_eq!(reply.header.ty, FrameType::Response, "{label}");
            }
        }
    }

    /// A legacy call with no preset reaches dispatch exactly as before presets
    /// existed: the same request, with the plugins' worker flag still the only
    /// thing that makes it a worker.
    #[tokio::test]
    async fn presetless_legacy_calls_dispatch_byte_identically() {
        let _guard = EXCHANGE_LOCK.lock().await;
        for flag in [false, true] {
            ACTIONS.store(0, Ordering::SeqCst);
            WORKER_ACTIONS.store(0, Ordering::SeqCst);
            LAST_DISPATCH.lock().unwrap().take();
            let mut body = json!({"name": "status", "arguments": {}});
            if flag {
                body["worker_session"] = json!(true);
            }
            let reply = exchange_with_scope(body, RouteRole::Legacy, "session", vec![], None).await;
            assert_eq!(reply.header.ty, FrameType::Response);
            assert_eq!(WORKER_ACTIONS.load(Ordering::SeqCst), usize::from(flag));
            let dispatched = LAST_DISPATCH.lock().unwrap().take().unwrap();
            let mut expected = json!({"command": "status", "session_id": "session", "params": {}});
            if flag {
                expected["params"]["worker_session"] = json!(true);
            }
            assert_eq!(dispatched, expected, "flag={flag}");
        }
    }

    #[tokio::test]
    async fn legacy_provider_calls_are_unsupported_without_actions() {
        let _guard = EXCHANGE_LOCK.lock().await;
        ACTIONS.store(0, Ordering::SeqCst);
        let mut failures = Vec::new();
        for op in ["tool.call", "tool.withdraw"] {
            let reply = exchange(
                json!({"op":op, "name":"status", "arguments":{}}),
                RouteRole::Legacy,
                "session",
                vec![],
            )
            .await;
            let actions = ACTIONS.load(Ordering::SeqCst);
            if reply.header.ty != FrameType::Error {
                failures.push(format!(
                    "{op}: expected Error unsupported_operation, got {:?}; actions={actions}",
                    reply.header.ty
                ));
            } else {
                let error: subc_protocol::ErrorBody = serde_json::from_slice(&reply.body).unwrap();
                if error.code != "unsupported_operation" || actions != 0 {
                    failures.push(format!("{op}: code={}, actions={actions}", error.code));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("; "));
    }

    #[tokio::test]
    async fn legacy_route_keeps_ordinary_management_and_opaque_pin_behavior() {
        let _guard = EXCHANGE_LOCK.lock().await;
        ACTIONS.store(0, Ordering::SeqCst);
        for body in [
            json!({"name":"status", "arguments":{}}),
            json!({"name":"status", "arguments":{}, "call_key":"legacy-key", "schema_pin":"opaque-pin"}),
        ] {
            let reply = exchange(body, RouteRole::Legacy, "session", vec![]).await;
            assert_eq!(reply.header.ty, FrameType::Response);
            let response: Value = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(response["isError"], false);
            assert_eq!(response["structuredContent"]["success"], true);
        }
        assert_eq!(ACTIONS.load(Ordering::SeqCst), 2);
        let reply = exchange(
            json!({"op": crate::commands::health_digest::HEALTH_DIGEST_OPERATION, "params":{}}),
            RouteRole::Legacy,
            "session",
            vec![],
        )
        .await;
        assert_eq!(reply.header.ty, FrameType::Response);
    }

    #[test]
    fn legacy_value_decoding_collapses_duplicate_fields_unlike_base_bytes() {
        let bytes = br#"{"name":"write","name":"status","arguments":{}}"#;
        assert!(serde_json::from_slice::<RouteRequest>(bytes).is_err());
        let envelope: Value = serde_json::from_slice(bytes).unwrap();
        let RouteRequest::ToolCall(call) = decode_legacy_route_request(envelope).unwrap() else {
            panic!("expected tool call")
        };
        assert_eq!(call.name, "status");
    }

    #[test]
    fn legacy_decoding_preserves_base_commit_identity() {
        // Legacy decoding uses op/params only after the untagged request fails.
        // A legacy schema_pin is an opaque token, not a parsed v1 schema pin.
        let health = crate::commands::health_digest::HEALTH_DIGEST_OPERATION;
        let cases = [
            (
                json!({"name":"status","arguments":{"extra":"雪"},"preview":true,"worker_session":true,"edit_slot_survives":true}),
                "status",
                json!({"extra":"雪"}),
                true,
            ),
            (
                json!({"op":health,"params":{"limit":3}}),
                health,
                json!({"limit":3}),
                false,
            ),
            (
                json!({"name":"status","arguments":{},"call_key":"legacy-key","schema_pin":"opaque-pin"}),
                "status",
                json!({}),
                false,
            ),
        ];
        for (body, name, arguments, flags) in cases {
            let decoded = decode_legacy_route_request(body.clone()).unwrap();
            let RouteRequest::ToolCall(call) = decoded else {
                panic!("expected tool call")
            };
            assert_eq!(call.name, name);
            assert_eq!(call.arguments, arguments);
            assert_eq!(call.preview, flags);
            assert_eq!(call.worker_session, flags);
            assert_eq!(call.edit_slot_survives, flags.then_some(true));
            assert_eq!(
                call.call_key.as_deref(),
                body.get("call_key").and_then(Value::as_str)
            );
            assert_eq!(
                call.schema_pin.as_deref(),
                body.get("schema_pin").and_then(Value::as_str)
            );
            if let Ok(base) =
                serde_json::from_slice::<RouteRequest>(&serde_json::to_vec(&body).unwrap())
            {
                assert_eq!(
                    format!("{base:?}"),
                    format!("{:?}", RouteRequest::ToolCall(call))
                );
            } else {
                assert_eq!(name, health);
            }
        }
        assert!(matches!(
            decode_legacy_route_request(json!({"op":"bg_events", "name":"status"})).unwrap(),
            RouteRequest::BgEvents(_)
        ));
    }

    #[tokio::test]
    async fn recognized_operations_and_v1_refusals_never_dispatch_legacy_actions() {
        let _guard = EXCHANGE_LOCK.lock().await;
        ACTIONS.store(0, Ordering::SeqCst);
        for op in [
            "role.describe",
            "tool.catalog",
            "tool.call",
            "tool.withdraw",
            "late_results",
            "late_results.ack",
        ] {
            for malformed in [false, true] {
                let body = json!({"op": op, "params": if malformed {json!(false)} else {json!({})}, "name": "write", "arguments": {"filePath": "action", "content": "mutate"}});
                let reply = exchange(body, RouteRole::Legacy, "session", vec![]).await;
                assert_eq!(
                    ACTIONS.load(Ordering::SeqCst),
                    0,
                    "{op} fell through to legacy"
                );
                if !matches!(op, "role.describe" | "tool.catalog") {
                    assert_eq!(reply.header.ty, FrameType::Error);
                }
            }
        }
        for name in ["bash_status", "bash_kill", "bash_write"] {
            for args in [
                json!({"task_id": "x"}),
                json!({"taskId": "x"}),
                json!({"task_id":"x","taskId":"x"}),
            ] {
                let reply = exchange(
                    json!({"name":name,"arguments":args}),
                    RouteRole::ToolProviderV1,
                    "session",
                    vec![name.into()],
                )
                .await;
                let error: subc_protocol::ErrorBody = serde_json::from_slice(&reply.body).unwrap();
                assert_eq!(error.code, "tool_disabled");
                assert_eq!(ACTIONS.load(Ordering::SeqCst), 0);
            }
        }
        let request = json!({"name":"write", "arguments":{"filePath":"action","content":"mutate","foreground_orchestrate":true}});
        let reply = exchange(request.clone(), RouteRole::ToolProviderV1, "", vec![]).await;
        assert_eq!(
            serde_json::from_slice::<subc_protocol::ErrorBody>(&reply.body)
                .unwrap()
                .detail
                .unwrap()["field"],
            "session"
        );
        let reply = exchange(
            request.clone(),
            RouteRole::ToolProviderV1,
            "session",
            vec![],
        )
        .await;
        assert_eq!(
            serde_json::from_slice::<subc_protocol::ErrorBody>(&reply.body)
                .unwrap()
                .detail
                .unwrap()["field"],
            "foreground_orchestrate"
        );
        assert_eq!(ACTIONS.load(Ordering::SeqCst), 0);
        // A pure legacy tool is deliberately allowed to carry extra arguments and
        // a missing session, without requiring a working database.
        let reply = exchange(
            json!({"name":"status","arguments":{"foreground_orchestrate":true}}),
            RouteRole::Legacy,
            "",
            vec![],
        )
        .await;
        assert_eq!(reply.header.ty, FrameType::Response);
        assert_eq!(ACTIONS.load(Ordering::SeqCst), 1);
    }
}
