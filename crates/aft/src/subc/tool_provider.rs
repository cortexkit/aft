//! The v1 catalog and admission boundary, separate from plugin tool forwarding.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use cortexkit_role_tool_provider::{
    call::{check_call, SchemaPin},
    catalog::{
        composition_digest, schema_digest, session_capabilities, system_text_digest, CatalogAnswer,
        CatalogRequest, CatalogTool, Reply, SystemTextAnswer,
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
            | "grants.list"
            | "grants.revoke"
    )
}

/// The call-ledger carrier a route principal records its calls under.
pub(super) fn ledger_carrier(principal: &crate::sandbox_spawn::AuthenticatedPrincipal) -> String {
    match principal {
        crate::sandbox_spawn::AuthenticatedPrincipal::RouteBind { principal_id, .. } => {
            principal_id.clone().unwrap_or_else(|| "absent".into())
        }
        crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty => "first-party".into(),
    }
}

pub(super) fn describe() -> Value {
    serde_json::to_value(
        RoleDescribe::new(
            vec![Major {
                version: "tool-provider/v1".into(),
                ops: vec![
                    "role.describe".into(),
                    "tool.catalog".into(),
                    "tool.call".into(),
                    "tool.withdraw".into(),
                    "late_results".into(),
                    "late_results.ack".into(),
                ],
                stability: "alpha".into(),
            }],
            env!("CARGO_PKG_VERSION"),
        )
        .with_capabilities(vec!["tool-catalog-confine/v1".into()]),
    )
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
    tools_for_session(
        CatalogPreset::Head,
        disabled,
        powershell_available,
        false,
        false,
    )
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
///
/// A session that may run commands on the remote runner (`runon`) also sees
/// bash's `runon` argument; no other session does. Likewise only a session
/// whose `revision_reads` is on sees the `ref` argument of read, outline and
/// zoom.
pub(super) fn tools_for_session(
    preset: CatalogPreset,
    disabled: &[String],
    powershell_available: bool,
    runon: bool,
    revision_reads: bool,
) -> Vec<CatalogTool> {
    served_tools(preset)
        .iter()
        .filter(|tool| powershell_available || tool.catalog.name != "powershell")
        .filter(|tool| crate::tool_gate::catalog_keeps(&tool.catalog.name, disabled))
        .filter(|tool| {
            tool.catalog.name != "bash_watch" || crate::tool_gate::catalog_keeps("bash", disabled)
        })
        .map(
            |tool| match (&tool.with_runon, runon, &tool.with_ref, revision_reads) {
                (Some(with_runon), true, _, _) => with_runon.clone(),
                (_, _, Some(with_ref), true) => with_ref.clone(),
                _ => tool.catalog.clone(),
            },
        )
        .collect()
}

// Schemas and semantics are embedded in the executable. Host availability and
// disabled-tool policy select from this immutable catalog; they do not change
// its schemas, so neither digests nor validators need request-scoped rebuilding.
struct ServedTool {
    catalog: CatalogTool,
    /// The same entry with bash's `runon` argument, served only to a session
    /// that may run commands remotely. Admission validates every call against
    /// it, so a call from either kind of session is checked the same way.
    with_runon: Option<CatalogTool>,
    /// The same entry with the `ref` (git revision) argument of read, outline
    /// and zoom, served only to a session whose `revision_reads` is on.
    /// Admission validates those calls against it, and the command refuses
    /// `ref` by name when the setting is off.
    with_ref: Option<CatalogTool>,
    /// Compiled once, when the catalog is built, and never during admission:
    /// the module admits calls on its frame loop's thread, whose stack the host
    /// may size as small as Windows' 1 MiB default, and compiling a JSON Schema
    /// recurses through the meta-schema.
    validator: jsonschema::Validator,
}

static SERVED_TOOLS: LazyLock<[Vec<ServedTool>; 3]> = LazyLock::new(|| {
    // The first caller to need a catalog is the first tool call, admitted on
    // the frame loop's thread. Build the catalog, and compile every admission
    // validator, on a thread with the ordinary default stack, so the frame loop
    // only ever iterates validators that are already compiled.
    std::thread::Builder::new()
        .name("aft-admission-validators".into())
        .spawn(|| {
            CatalogPreset::ALL.map(|preset| {
                build_tools(preset)
                    .into_iter()
                    .map(|(catalog, with_runon, with_ref)| {
                        let tool = with_runon
                            .as_ref()
                            .or(with_ref.as_ref())
                            .unwrap_or(&catalog);
                        #[cfg(test)]
                        ADMISSION_WORK.with(|count| {
                            let (catalogs, validators) = count.get();
                            count.set((catalogs, validators + 1));
                        });
                        let validator = jsonschema::validator_for(&tool.input_schema)
                            .expect("embedded schemas are valid JSON schemas");
                        ServedTool {
                            catalog,
                            with_runon,
                            with_ref,
                            validator,
                        }
                    })
                    .collect()
            })
        })
        .expect("spawning the admission-validator builder")
        .join()
        .expect("embedded tool catalog builds on a worker")
});

fn served_tools(preset: CatalogPreset) -> &'static [ServedTool] {
    &SERVED_TOOLS[match preset {
        CatalogPreset::Head => 0,
        CatalogPreset::Worker => 1,
        CatalogPreset::Reader => 2,
    }]
}

type ServedVariants = (CatalogTool, Option<CatalogTool>, Option<CatalogTool>);

fn build_tools(preset: CatalogPreset) -> Vec<ServedVariants> {
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
        // The manifest's descriptions drop references to sibling tools, since a
        // manifest consumer may take any subset; this catalog knows its exact
        // tool set, so it starts from the full text and renders it for that set.
        .map(|tool| {
            let description = manifest::tool_description(&tool.name);
            (tool.name, tool.schema, description)
        })
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
        .map(|(name, schema, description)| {
            let (_, tag, restrict_replace) = METADATA
                .iter()
                .chain(PRESET_ONLY_METADATA)
                .find(|(known, _, _)| *known == name)
                .expect("every served tool has v1 metadata");
            let entry = |mut schema: Value, description: Option<String>| {
                schema
                    .as_object_mut()
                    .expect("tool schemas are objects")
                    .remove("description");
                let digest =
                    schema_digest(&schema).expect("embedded schema has a structural digest");
                let semantics = if name == "bash"
                    || (preset == CatalogPreset::Worker && name == "bash_watch")
                {
                    2
                } else {
                    1
                };
                let mut entry = CatalogTool::new(name.clone(), digest, semantics, schema);
                let mut modes = vec![cortexkit_role_tool_provider::CallMode::Classic];
                if name != "bash_watch" {
                    modes.push(cortexkit_role_tool_provider::CallMode::Codemode);
                }
                if matches!(
                    name.as_str(),
                    "read" | "grep" | "glob" | "search" | "outline" | "zoom" | "callgraph"
                ) {
                    modes.push(cortexkit_role_tool_provider::CallMode::Flow);
                }
                entry.modes = Some(modes);
                if !tag.is_empty() {
                    entry.capabilities.push((*tag).into());
                }
                entry.description = description;
                if *restrict_replace {
                    entry.result_ops = Some(vec!["prepend".into(), "append".into()]);
                }
                entry
            };
            let preset_name = (preset != CatalogPreset::Head).then(|| preset.name());
            let with_runon = manifest::remote_tool(preset_name, &name)
                .map(|(schema, description)| entry(schema, description));
            let with_ref = manifest::revision_tool(&name)
                .map(|(schema, description)| entry(schema, description));
            (entry(schema, description), with_runon, with_ref)
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

/// System instructions for the primary (`head`) session. The legacy `broca`
/// text request can set `worker` to append a reminder to stay in scope.
pub(super) fn broca_text(names: &[&str], worker: bool) -> String {
    let has = |name: &str| names.contains(&name);
    let mut text = String::from("# Agent File Tools\n\n");
    code_tool_lines(&mut text, has);
    if has("bash") {
        text.push_str("Use bash with wait: true for long commands. The provider waits for completion, the command deadline, or bash.worker_wait_max_ms (30 minutes by default), then hands back a still-running task; no completion subscriber is required.\n");
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

#[cfg(test)]
pub(super) fn catalog(
    body: Value,
    disabled: &[String],
    powershell_available: bool,
) -> Result<Value, ErrorBody> {
    catalog_for_session(
        body,
        disabled,
        powershell_available,
        false,
        crate::config::DEFAULT_BASH_WORKER_WAIT_MAX_MS,
    )
}

// A 1,800,006 ms bash_watch execution lost its reply to Broca's 1,830 s
// transport timeout: the old 30 s allowance did not cover dispatch and delivery
// delay outside AFT's execution clock. Declare four minutes beyond the wait so
// the reply-budget safety reserve preserves a full default 30-minute wait.
// This extends the caller's reply window, never the command's hard timeout.
pub(crate) const WORKER_REPLY_MARGIN_MS: u64 = 240_000;
const MAX_DECLARED_REPLY_MS: u64 = 86_400_000;

/// Render a catalog directly from arguments for catalog conformance fixtures.
#[cfg(test)]
pub(super) fn catalog_for_session(
    body: Value,
    disabled: &[String],
    powershell_available: bool,
    runon: bool,
    worker_wait_max_ms: u64,
) -> Result<Value, ErrorBody> {
    let (request, preset) = validate_catalog_request(body)?;
    catalog_for_request(
        request,
        preset,
        disabled,
        powershell_available,
        runon,
        false,
        worker_wait_max_ms,
    )
}

pub(super) fn validate_catalog_request(
    body: Value,
) -> Result<(CatalogRequest, CatalogPreset), ErrorBody> {
    let request: CatalogRequest = serde_json::from_value(body)
        .map_err(|e| errors::invalid_request("arguments", e.to_string()))?;
    // The tool-provider contract reads a system-text item's preset as
    // optional, where a missing preset asks for the provider's default text.
    // AFT does not offer default text (its manifest does not declare
    // system-text-default/v1), so it keeps refusing such an item with the
    // error the contract's decoder gave while the preset was required. That
    // decoder refused it before any other check ran, so this check runs first.
    if request
        .system_text
        .as_ref()
        .is_some_and(|item| item.preset.is_none())
    {
        return Err(errors::invalid_request(
            "arguments",
            "missing field `preset`",
        ));
    }
    let preset = CatalogPreset::parse("preset", request.preset.as_deref())?;
    if let Some((key, _)) = request.params.iter().next() {
        return Err(errors::invalid_request(
            &format!("params.{key}"),
            "unsupported catalog parameter",
        ));
    }
    if let Some(item) = &request.system_text {
        // Checked above: a system-text item without a preset never gets here.
        let text_preset = item.preset.as_deref().unwrap_or_default();
        if text_preset != "broca" && text_preset != preset.name() {
            return Err(errors::invalid_request(
                "system_text.preset",
                format!(
                    "AFT serves the {:?} system text beside the {:?} catalog preset, or the legacy \"broca\" text",
                    preset.name(), preset.name()
                ),
            ));
        }
        for (key, value) in &item.params {
            if key != "worker" || !value.is_boolean() || text_preset != "broca" {
                return Err(errors::invalid_request(
                    &format!("system_text.params.{key}"),
                    "unsupported system-text parameter",
                ));
            }
        }
    }
    Ok((request, preset))
}

#[cfg(test)]
thread_local! {
    pub(super) static CATALOG_RENDERS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Render validated arguments under the session's resolved remote policy and
/// blocking wait limit. Only remotely enabled sessions see bash's `runon`
/// argument, and only sessions with `revision_reads` on see `ref`.
pub(super) fn catalog_for_request(
    request: CatalogRequest,
    preset: CatalogPreset,
    disabled: &[String],
    powershell_available: bool,
    runon: bool,
    revision_reads: bool,
    worker_wait_max_ms: u64,
) -> Result<Value, ErrorBody> {
    render_catalog(
        request,
        preset,
        disabled,
        powershell_available,
        (runon, revision_reads),
        worker_wait_max_ms,
        CatalogRestrictions::default(),
    )
}

pub(super) fn catalog_for_confined_request(
    request: CatalogRequest,
    preset: CatalogPreset,
    disabled: &[String],
    powershell_available: bool,
    runon: bool,
    revision_reads: bool,
    worker_wait_max_ms: u64,
) -> Result<Value, ErrorBody> {
    render_catalog(
        request,
        preset,
        disabled,
        powershell_available,
        (runon, revision_reads),
        worker_wait_max_ms,
        CatalogRestrictions {
            confined: true,
            rich_ui: false,
        },
    )
}

pub(super) fn catalog_for_rich_ui_request(
    request: CatalogRequest,
    preset: CatalogPreset,
    disabled: &[String],
    revision_reads: bool,
    worker_wait_max_ms: u64,
    confined: bool,
) -> Result<Value, ErrorBody> {
    render_catalog(
        request,
        preset,
        disabled,
        false,
        (false, revision_reads),
        worker_wait_max_ms,
        CatalogRestrictions {
            confined,
            rich_ui: true,
        },
    )
}

/// How a generated description spells a reference to another AFT tool. The
/// catalog descriptions are generated from the OpenCode plugin's tools, which
/// that plugin registers as `aft_<name>`; a catalog serves them as `<name>`
/// (or `aft_outline` on a confined route), and may leave some out.
const TOOL_REFERENCE_PREFIX: &str = "aft_";

/// The phrases a description uses to name a tool by its bare name, as
/// (text before the name, text after it): "the `edit` tool", "the grep tool",
/// "use `read` for that". A bare name anywhere else is not a tool reference:
/// `callgraph` is also one of zoom's arguments, and "read" and "edit" are
/// ordinary words.
const BARE_TOOL_REFERENCE_FORMS: [(&str, &str); 4] = [
    ("the `", "` tool"),
    ("the ", " tool"),
    ("use `", "`"),
    ("Use `", "`"),
];

/// Rewrites every served description for the tool set this catalog serves.
/// A reference to a served tool takes the name the tool is served under. A
/// reference to a tool this catalog does not serve (the preset leaves it out,
/// or the session disabled it) is removed together with the smallest stretch
/// of text that still reads on its own without it: a parenthetical, then a
/// `;` clause, then a sentence, then the whole line. A model must never be
/// steered to a tool it was not offered, or by a name it cannot call.
fn render_tool_references(tools: &mut [CatalogTool]) {
    let served: Vec<String> = tools.iter().map(|tool| tool.name.clone()).collect();
    for tool in tools.iter_mut() {
        if let Some(description) = tool.description.as_mut() {
            *description = resolve_tool_references(description, &served);
        }
    }
}

/// One reference to an AFT tool: the byte range of its name in the text.
struct ToolReference {
    start: usize,
    end: usize,
    /// The name the tool is served under, or `None` when this catalog does
    /// not serve it.
    served: Option<String>,
}

/// Whether `at` is at the start of `text` or follows a character other than
/// an ASCII letter, digit or underscore.
fn starts_word(text: &str, at: usize) -> bool {
    text[..at]
        .chars()
        .next_back()
        .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
}

/// Every reference in `text` to a tool AFT defines, in text order: an
/// `aft_<name>` spelling, or a bare name in one of
/// [`BARE_TOOL_REFERENCE_FORMS`]. `aft_` followed by any other name is
/// ordinary text and is left alone.
fn tool_references(text: &str, served: &[String]) -> Vec<ToolReference> {
    let mut found = Vec::new();
    for (name, _, _) in METADATA.iter().chain(PRESET_ONLY_METADATA) {
        for (before, after) in BARE_TOOL_REFERENCE_FORMS {
            for (at, _) in text.match_indices(&format!("{before}{name}{after}")) {
                if !starts_word(text, at) {
                    continue;
                }
                let start = at + before.len();
                found.push(ToolReference {
                    start,
                    end: start + name.len(),
                    served: served
                        .iter()
                        .find(|served| {
                            served.as_str() == *name
                                || served.strip_prefix(TOOL_REFERENCE_PREFIX) == Some(*name)
                        })
                        .cloned(),
                });
            }
        }
    }
    for (start, _) in text.match_indices(TOOL_REFERENCE_PREFIX) {
        if !starts_word(text, start) {
            continue;
        }
        let name_start = start + TOOL_REFERENCE_PREFIX.len();
        let end = text[name_start..]
            .find(|c: char| !(c.is_ascii_lowercase() || c == '_'))
            .map_or(text.len(), |len| name_start + len);
        let name = &text[name_start..end];
        if !METADATA
            .iter()
            .chain(PRESET_ONLY_METADATA)
            .any(|(known, _, _)| *known == name)
        {
            continue;
        }
        let spelled = &text[start..end];
        found.push(ToolReference {
            start,
            end,
            served: served
                .iter()
                .find(|served| served.as_str() == name || served.as_str() == spelled)
                .cloned(),
        });
    }
    found.sort_by_key(|reference| reference.start);
    found
}

fn names_absent_tool(text: &str, served: &[String]) -> bool {
    tool_references(text, served)
        .iter()
        .any(|reference| reference.served.is_none())
}

/// [`render_tool_references`] for one description.
pub(super) fn resolve_tool_references(text: &str, served: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    // Whether the line before was removed whole; a blank line right after it
    // goes too when the text already ends in a blank line (or is still empty),
    // so removing a paragraph leaves one paragraph break, not two.
    let mut after_removed_line = false;
    for line in text.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(body) => (body, "\n"),
            None => (line, ""),
        };
        if after_removed_line && body.trim().is_empty() && (out.is_empty() || out.ends_with("\n\n"))
        {
            continue;
        }
        after_removed_line = false;
        if !names_absent_tool(body, served) {
            out.push_str(&rename_served_tools(body, served));
            out.push_str(newline);
            continue;
        }
        let kept = drop_absent_sentences(body, served);
        let kept = kept.trim_end();
        // A line left with only its indentation or list marker goes whole.
        if kept
            .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '-' | '*' | '•'))
            .is_empty()
        {
            after_removed_line = true;
            continue;
        }
        out.push_str(&rename_served_tools(kept, served));
        out.push_str(newline);
    }
    // A removed last line leaves the line break (or blank line) before it.
    if after_removed_line {
        let kept = out.trim_end_matches('\n').len();
        out.truncate(kept);
        if text.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// `line` without the sentences, clauses or parentheticals that name a tool
/// this catalog does not serve. A sentence ends at ". ".
fn drop_absent_sentences(line: &str, served: &[String]) -> String {
    let mut sentences = Vec::new();
    let mut start = 0;
    for (at, separator) in line.match_indices(". ") {
        sentences.push(&line[start..at + separator.len()]);
        start = at + separator.len();
    }
    if start < line.len() {
        sentences.push(&line[start..]);
    }
    let mut out = String::with_capacity(line.len());
    for sentence in sentences {
        if !names_absent_tool(sentence, served) {
            out.push_str(sentence);
            continue;
        }
        let sentence = drop_absent_parentheticals(sentence, served);
        let sentence = drop_absent_dash_lead(&sentence, served);
        if !names_absent_tool(&sentence, served) {
            out.push_str(&sentence);
        } else if let Some(kept) = drop_absent_clauses(&sentence, served) {
            out.push_str(&kept);
        }
    }
    out
}

/// `sentence` without its lead when only the part before its first " — "
/// names an absent tool: "To overwrite a file, use the `write` tool — `edit`
/// requires an explicit mode." keeps what follows the dash. An absent tool
/// after the dash removes nothing here, because the lead may depend on it
/// ("STOP — use the grep tool instead.").
fn drop_absent_dash_lead(sentence: &str, served: &[String]) -> String {
    let Some(at) = sentence.find(" — ") else {
        return sentence.to_string();
    };
    let (lead, tail) = (&sentence[..at], &sentence[at + " — ".len()..]);
    if !names_absent_tool(lead, served) || names_absent_tool(tail, served) {
        return sentence.to_string();
    }
    let indent = &lead[..lead.len() - lead.trim_start().len()];
    let mut tail = tail.to_string();
    if let Some(first) = tail.chars().next() {
        if first.is_ascii_lowercase() && !tail.starts_with(TOOL_REFERENCE_PREFIX) {
            tail.replace_range(..1, &first.to_ascii_uppercase().to_string());
        }
    }
    format!("{indent}{tail}")
}

/// `sentence` without each parenthetical that names an absent tool, and the
/// space before it.
fn drop_absent_parentheticals(sentence: &str, served: &[String]) -> String {
    let mut sentence = sentence.to_string();
    loop {
        let Some(reference) = tool_references(&sentence, served)
            .into_iter()
            .find(|reference| reference.served.is_none())
        else {
            return sentence;
        };
        let open = sentence[..reference.start].rfind('(');
        let close = sentence[reference.end..]
            .find(')')
            .map(|at| reference.end + at);
        let (Some(open), Some(close)) = (open, close) else {
            return sentence;
        };
        if sentence[open..reference.start].contains(')')
            || sentence[reference.end..close].contains('(')
        {
            return sentence;
        }
        let from = if sentence[..open].ends_with(' ') {
            open - 1
        } else {
            open
        };
        sentence.replace_range(from..=close, "");
    }
}

/// `sentence` without its `;` clauses that name an absent tool, keeping the
/// sentence's own ending; `None` when no clause is left.
fn drop_absent_clauses(sentence: &str, served: &[String]) -> Option<String> {
    let body = sentence.trim_end_matches(['.', ' ']);
    let ending = &sentence[body.len()..];
    let indent = &body[..body.len() - body.trim_start().len()];
    let clauses: Vec<&str> = body.trim_start().split("; ").collect();
    let kept: Vec<&str> = clauses
        .iter()
        .copied()
        .filter(|clause| !names_absent_tool(clause, served))
        .collect();
    if kept.is_empty() {
        return None;
    }
    let mut joined = kept.join("; ");
    // The sentence now starts at what was a later clause. Capitalize it,
    // unless it starts with a tool name, which keeps its own spelling.
    if names_absent_tool(clauses[0], served) && !joined.starts_with(TOOL_REFERENCE_PREFIX) {
        if let Some(first) = joined.chars().next() {
            if first.is_ascii_lowercase() {
                joined.replace_range(..1, &first.to_ascii_uppercase().to_string());
            }
        }
    }
    Some(format!("{indent}{joined}{ending}"))
}

/// `text` with each reference to a served tool spelled as it is served.
fn rename_served_tools(text: &str, served: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for reference in tool_references(text, served) {
        if let Some(name) = reference.served {
            let before = &text[copied..reference.start];
            // "an aft_safety checkpoint" becomes "a safety checkpoint".
            let article = before
                .strip_suffix("an ")
                .or_else(|| before.strip_suffix("An "))
                .filter(|rest| rest.is_empty() || rest.ends_with(' '));
            match article {
                Some(rest) if !name.starts_with(['a', 'e', 'i', 'o', 'u']) => {
                    out.push_str(rest);
                    out.push_str(&before[rest.len()..rest.len() + 1]);
                    out.push(' ');
                }
                _ => out.push_str(before),
            }
            out.push_str(&name);
            copied = reference.end;
        }
    }
    out.push_str(&text[copied..]);
    out
}

#[derive(Default)]
struct CatalogRestrictions {
    confined: bool,
    rich_ui: bool,
}

/// `offers` is the session's (`runon`, `revision_reads`) pair: which
/// enabled-only arguments its catalog carries.
fn render_catalog(
    request: CatalogRequest,
    preset: CatalogPreset,
    disabled: &[String],
    powershell_available: bool,
    offers: (bool, bool),
    worker_wait_max_ms: u64,
    restrictions: CatalogRestrictions,
) -> Result<Value, ErrorBody> {
    #[cfg(test)]
    CATALOG_RENDERS.set(CATALOG_RENDERS.get() + 1);
    let (runon, revision_reads) = offers;
    let mut answer = CatalogAnswer::new("", "").with_tools(tools_for_session(
        preset,
        disabled,
        powershell_available,
        runon,
        revision_reads,
    ));
    if request.preset.is_none() {
        answer = answer.with_presets(CatalogPreset::ALL.map(CatalogPreset::name));
    }
    if restrictions.rich_ui {
        // Head catalogs normally omit the worker-only watch tool. A UI may
        // observe an existing task without gaining permission to start one.
        if preset == CatalogPreset::Head {
            answer.tools.extend(
                tools_for_session(
                    CatalogPreset::Worker,
                    disabled,
                    false,
                    false,
                    revision_reads,
                )
                .into_iter()
                .filter(|tool| tool.name == "bash_watch"),
            );
        }
        answer
            .tools
            .retain(|tool| super::carrier_read_only::allowed(&tool.name));
        for tool in &mut answer.tools {
            if let Some(canonical) = crate::tool_gate::canonical_tool_name(&tool.name) {
                tool.name = canonical.into();
            }
        }
    }
    if restrictions.confined {
        answer.tools.retain(|tool| {
            matches!(
                tool.name.as_str(),
                "read" | "grep" | "glob" | "outline" | "aft_outline"
            )
        });
        for tool in &mut answer.tools {
            if tool.name == "outline" {
                tool.name = "aft_outline".into();
            }
        }
    }
    // Last, once the served names are final: every description names the
    // tools of this catalog and no other.
    render_tool_references(&mut answer.tools);
    declare_result_format(&mut answer.tools, super::result_body::DECLARE_RESULT_FORMAT);
    // Every preset can settle a keyed call after its reply (a held shell
    // call, or a scoped call whose end was never observed), so every catalog
    // declares the session capability that lets a custodian pull those
    // results.
    answer
        .capabilities
        .insert(session_capabilities::LATE_RESULTS.into(), json!(true));
    if matches!(preset, CatalogPreset::Head | CatalogPreset::Worker) {
        match worker_wait_max_ms.checked_add(WORKER_REPLY_MARGIN_MS)
            .filter(|max_ms| *max_ms <= MAX_DECLARED_REPLY_MS)
        {
            Some(max_ms) => {
                for tool in &mut answer.tools {
                    if matches!(tool.name.as_str(), "bash" | "bash_watch") {
                        tool.reply = Some(Reply::new(max_ms));
                    }
                }
            }
            None => log::info!(
                "{} catalog: bash.worker_wait_max_ms={worker_wait_max_ms}; reply omitted because the cap plus {WORKER_REPLY_MARGIN_MS}ms margin exceeds the 24-hour tool-provider deadline maximum", preset.name()
            ),
        }
    }
    let composition = request.composition.as_ref().map(|composition| {
        composition_digest(&Value::Object(composition.clone())).expect("composition is JSON")
    });
    answer.composition_digest = composition.clone();
    if let Some(item) = request.system_text {
        // The request was validated before policy persistence; rendering must
        // not introduce a refusal after the routing settings have been frozen.
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
    // nothing: the preset-less answer serves head's tools, but its discovery
    // list is additional content and participates in the fingerprint above.
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

pub(super) fn declare_result_format(tools: &mut [CatalogTool], enabled: bool) {
    if enabled {
        for tool in tools {
            tool.result_format = Some(super::result_body::RESULT_FORMAT.to_string().into());
        }
    }
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
pub(super) fn admit_call_mode(
    name: &str,
    mode: &cortexkit_role_tool_provider::CallMode,
) -> Result<(), ErrorBody> {
    use cortexkit_role_tool_provider::{catalog::serves_call_mode, CallMode};
    let tool = served_tools(CatalogPreset::Head)
        .iter()
        .find(|tool| tool.catalog.name == name)
        .or_else(|| {
            served_tools(CatalogPreset::Worker)
                .iter()
                .find(|tool| tool.catalog.name == name)
        });
    if tool.is_some_and(|tool| serves_call_mode(&tool.catalog, mode))
        || (*mode == CallMode::Classic && tool.is_none())
    {
        return Ok(());
    }
    let mut error = ErrorBody::new(
        cortexkit_role_tool_provider::errors::CALL_MODE_UNSUPPORTED,
        "tool does not serve the bind's call mode",
    );
    error.detail = Some(json!({"tool":name, "mode":mode}));
    Err(error)
}

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

// A keyed shell may ask an untrusted consumer for permission, but the
// consumer's elicitation capability does not authorize shell observation.
pub(super) fn admission_trusted(
    identity: &super::RouteIdentity,
    call: &cortexkit_role_tool_provider::call::ToolCallRequest,
) -> bool {
    !matches!(identity.trust, super::BindTrust::Untrusted)
        || super::carrier_read_only::observes_tasks(identity, &call.name)
        || (call.call_key.is_some()
            && identity.consumer_elicitation_capable
            && matches!(call.name.as_str(), "bash" | "powershell"))
}

/// Shell capability refusals need no saved policy or database access.
pub(super) fn shell_capability_refusal(name: &str, trusted: bool) -> Option<ErrorBody> {
    (!trusted
        && matches!(
            name,
            "bash" | "powershell" | "bash_status" | "bash_kill" | "bash_write" | "bash_watch"
        ))
    .then(|| {
        ErrorBody::new(
            errors::CAPABILITY_NOT_ADMITTED,
            "shell execution and observation require an admitted principal",
        )
    })
}

pub(super) fn ledger_scope(
    scope: &subc_protocol::scope::ScopeStamp,
) -> crate::db::call_ledger::ScopeIdentity {
    crate::db::call_ledger::ScopeIdentity {
        owner: super::principal_id(&Some(scope.owner.clone())).unwrap(),
        scope_ref: scope.scope_ref.clone(),
        scope_epoch: scope.scope_epoch,
    }
}

pub(super) fn keyed_route_frame(
    decoded: &super::DecodedFrame,
    routes: &std::collections::HashMap<super::RouteChannel, super::RouteIdentity>,
) -> bool {
    let frame = &decoded.frame;
    routes
        .get(&super::route_key(frame.header.channel, frame.header.epoch))
        .is_some_and(|identity| identity.role == RouteRole::ToolProviderV1)
        && decoded
            .body()
            .is_ok_and(|body| body.get("call_key").and_then(Value::as_str).is_some())
}

pub(super) fn note_native_outcome(
    ctx: &crate::context::AppContext,
    key: Option<&crate::db::call_ledger::Key>,
    response: &crate::protocol::Response,
) {
    if let (Some(key), Some(db)) = (key, ctx.db()) {
        if let Ok(conn) = db.lock() {
            if let Err(error) = crate::db::call_ledger::note_native_outcome(&conn, key, response) {
                log::warn!("call ledger outcome recording failed: {error}");
            }
        }
    }
}

/// The edge parks admission replies outside the frame loop. The executor
/// commits admission before returning a frame to dispatch; terminal frames
/// pass through the same edge and are committed before reaching the socket.
pub(super) struct LedgerEdge {
    calls: std::sync::Mutex<std::collections::HashMap<(super::RouteChannel, u64), LedgerCall>>,
    waiting: std::sync::Mutex<std::collections::HashMap<(super::RouteChannel, u64), LedgerWait>>,
    authorizing: std::sync::Mutex<std::collections::HashSet<super::ReverseCorrKey>>,
    ready: tokio::sync::mpsc::UnboundedSender<super::DecodedFrame>,
    recent_sweeps: std::sync::Mutex<
        std::collections::HashMap<crate::path_identity::ProjectRootId, std::time::Instant>,
    >,
}

#[cfg(test)]
thread_local! {
    // For cloned request JSON and permission arguments, count encoded bytes,
    // not estimated heap allocations. Response-body copies count vector lengths.
    // Include the request clone used to construct the owned ToolCallRequest.
    static KEYED_COPY_BYTES: std::cell::Cell<[usize; 3]> = const { std::cell::Cell::new([0; 3]) };
}

fn copy_call_json(value: &Value, kind: usize) -> Value {
    #[cfg(test)]
    KEYED_COPY_BYTES.with(|work| {
        let mut bytes = work.get();
        bytes[kind] += serde_json::to_vec(value).unwrap().len();
        work.set(bytes);
    });
    #[cfg(not(test))]
    let _ = kind;
    value.clone()
}

fn copy_terminal_body(body: &[u8]) -> Vec<u8> {
    #[cfg(test)]
    KEYED_COPY_BYTES.with(|work| {
        let mut bytes = work.get();
        bytes[2] += body.len();
        work.set(bytes);
    });
    body.to_vec()
}
struct LedgerWait {
    admission: super::AdmissionWait,
    cancel: super::PersistentCancelSignal,
    terminal_claim: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

struct LedgerCall {
    root: crate::path_identity::ProjectRootId,
    key: crate::db::call_ledger::Key,
    shell: bool,
    detached: bool,
    expires_at: std::time::Instant,
    /// A `tool.withdraw` settled this held call before the frame loop filed
    /// its question; the request is answered `not_started` when it arrives.
    withdrawn: Option<&'static str>,
}

/// Test-only gate between a keyed call's durable admission and the frame
/// loop seeing its request (which, for a held shell call, files the
/// permission question). Lets a real-process test withdraw the call in that
/// window. With `AFT_TEST_LEDGER_ADMISSION_GATE` naming a path and a file at
/// that path, the admission writes `<path>.held` and waits, at most 60 s,
/// until the file is removed. Compiled only with `test-timing-hooks`.
#[cfg(feature = "test-timing-hooks")]
async fn admission_gate() {
    let Some(gate) = std::env::var_os("AFT_TEST_LEDGER_ADMISSION_GATE") else {
        return;
    };
    let gate = std::path::PathBuf::from(gate);
    if !gate.exists() {
        return;
    }
    let mut held = gate.clone().into_os_string();
    held.push(".held");
    if std::fs::write(&held, b"held").is_err() {
        return;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while gate.exists() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
#[cfg(not(feature = "test-timing-hooks"))]
async fn admission_gate() {}

/// The role operations that read or change the call ledger.
fn custody_operation(name: &str) -> bool {
    matches!(name, "tool.withdraw" | "late_results" | "late_results.ack")
}

/// The largest body a role reply may carry: one subc frame. Withdraw answers
/// and late-results pages are bounded by it, never by the tool-response limit.
pub(super) const ROLE_REPLY_BODY_LIMIT: usize = subc_protocol::MAX_FRAME_BODY_LEN as usize;

/// A custody request whose caller checks passed and that now needs the ledger.
enum Custody {
    Withdraw {
        key: crate::db::call_ledger::Key,
        /// The scope's owner named no carrier and is treated as the carrier;
        /// finding no record of its own is then a refusal, not `unknown_call`.
        owner_as_carrier: bool,
    },
    LateResults(cortexkit_role_tool_provider::late_results::LateResultsRequest),
    Ack(cortexkit_role_tool_provider::late_results::AckRequest),
}

/// The canonical principal spelling of the route's verified caller: the same
/// rendering the ledger keys carriers by and scope stamps name owners by.
fn route_principal(identity: &super::RouteIdentity) -> String {
    LedgerEdge::key(identity, "").carrier
}

fn role_scope(
    scope: &crate::db::call_ledger::ScopeIdentity,
) -> cortexkit_role_tool_provider::scope::ScopeIdentity {
    cortexkit_role_tool_provider::scope::ScopeIdentity {
        owner: scope.owner.clone(),
        scope_ref: scope.scope_ref.clone(),
        scope_epoch: scope.scope_epoch,
    }
}

/// The caller checks and argument decoding of a custody request, which need
/// no ledger access. A failure is the route error to answer with.
fn custody_request(
    op: &str,
    body: &Value,
    identity: &super::RouteIdentity,
) -> Result<Custody, ErrorBody> {
    use cortexkit_role_tool_provider::{
        late_results::{AckRequest, LateResultsRequest},
        withdraw::{parse_withdraw_request, resolve_carrier, CarrierResolution},
    };
    let arguments = || body.get("arguments").cloned().unwrap_or_else(|| json!({}));
    match op {
        "tool.withdraw" => {
            let request: cortexkit_role_tool_provider::call::ToolCallRequest =
                serde_json::from_value(body.clone())
                    .map_err(|error| errors::invalid_request("arguments", error.to_string()))?;
            let arguments = parse_withdraw_request(&request)?;
            let caller = route_principal(identity);
            let stamp = identity
                .scope
                .as_ref()
                .map(|scope| role_scope(&ledger_scope(scope)));
            let (carrier, owner_as_carrier) =
                match resolve_carrier(&caller, stamp.as_ref(), &arguments)
                    .map_err(|problem| problem.error())?
                {
                    CarrierResolution::Carrier(carrier) => (carrier, false),
                    CarrierResolution::OwnerAsCarrier(owner) => (owner, true),
                };
            Ok(Custody::Withdraw {
                key: crate::db::call_ledger::Key {
                    carrier,
                    call_key: arguments.call_key,
                },
                owner_as_carrier,
            })
        }
        "late_results" => serde_json::from_value::<LateResultsRequest>(arguments())
            .map(Custody::LateResults)
            .map_err(|error| errors::invalid_request("arguments", error.to_string())),
        _ => serde_json::from_value::<AckRequest>(arguments())
            .map(Custody::Ack)
            .map_err(|error| errors::invalid_request("arguments", error.to_string())),
    }
}

/// What a custody executor job produced: the reply body, and whether a held
/// call was withdrawn so its request must be ended.
struct CustodyReply {
    body: Vec<u8>,
    withdrawn_key: Option<crate::db::call_ledger::Key>,
    /// A withdrawn running call whose task this root's registry does not
    /// hold. `body` is its answer, not stored yet: the kill must run on the
    /// root that owns the task before the answer is stored and sent.
    kill_elsewhere: Option<WithdrawKill>,
}

/// The kill a withdraw still owes, and the answer to store once it lands.
struct WithdrawKill {
    key: crate::db::call_ledger::Key,
    task: String,
    body: Vec<u8>,
}

/// The refusal for a withdraw whose running call could not be stopped. No
/// answer was stored, so retrying the withdraw tries the kill again.
fn withdraw_kill_failed(task: &str, error: &str) -> ErrorBody {
    ErrorBody::new(
        "withdraw_incomplete",
        format!(
            "the withdrawn call's task {task} could not be stopped: {error}; retry tool.withdraw"
        ),
    )
    .with_detail(json!({"retryable": true, "task_id": task}))
}

/// Store a withdraw's answer once its kill landed (or was not needed), and
/// return the answer every later withdraw will get.
fn freeze_withdraw_answer(
    ctx: &crate::context::AppContext,
    kill: &WithdrawKill,
) -> Result<Vec<u8>, ErrorBody> {
    let unavailable = |message: String| {
        ErrorBody::new("database_unavailable", message).with_detail(json!({"retryable": false}))
    };
    let db = ctx
        .db()
        .ok_or_else(|| unavailable("call ledger is unavailable".into()))?;
    let conn = db
        .lock()
        .map_err(|_| unavailable("call ledger database mutex is poisoned".into()))?;
    crate::db::call_ledger::freeze_withdraw_answer(&conn, &kill.key, &kill.body)
        .map_err(|error| unavailable(error.to_string()))
}

/// Stop a withdrawn running call's task through `ctx`'s task registry (each
/// project root has its own), then store the withdraw's answer. When that
/// registry does not hold the task, the answer is stored only if the task's
/// record shows it has already stopped; otherwise nothing is stored and the
/// withdraw is refused as retryable.
fn kill_withdrawn_call(
    ctx: &crate::context::AppContext,
    kill: &WithdrawKill,
) -> Result<Vec<u8>, ErrorBody> {
    if ctx.bash_background().holds_task(&kill.task) {
        // The ledger lock must not be held here: killing persists the task's
        // terminal state through the same `aft.db` connection.
        if let Err(error) = ctx.bash_background().kill_withdrawn(&kill.task) {
            log::warn!("withdrawn call task {} was not killed: {error}", kill.task);
            return Err(withdraw_kill_failed(&kill.task, &error));
        }
        return freeze_withdraw_answer(ctx, kill);
    }
    let stopped = ctx.db().is_some_and(|db| {
        db.lock().ok().is_some_and(|conn| {
            crate::db::bash_tasks::bash_task_stopped(&conn, &kill.task).unwrap_or(false)
        })
    });
    if stopped {
        return freeze_withdraw_answer(ctx, kill);
    }
    log::warn!(
        "withdrawn call task {} was not killed: no bound project root holds it",
        kill.task
    );
    Err(withdraw_kill_failed(
        &kill.task,
        "no bound project root holds the task",
    ))
}

/// Runs a custody request against the route root's ledger, on the actor's
/// own `aft.db` connection.
fn run_custody(
    ctx: &crate::context::AppContext,
    request: Custody,
    custodian: &str,
    stamp: Option<&crate::db::call_ledger::ScopeIdentity>,
    request_id: &str,
) -> Result<CustodyReply, ErrorBody> {
    use crate::db::call_ledger::{self as ledger, Withdrawal};
    use cortexkit_role_tool_provider::withdraw::{CallerProblem, WithdrawAnswer};
    if let Some(refusal) = ctx.database_runtime_refusal(request_id, "bash") {
        return Err(ErrorBody::new(
            refusal.data["code"]
                .as_str()
                .unwrap_or("database_unavailable"),
            refusal.data["message"]
                .as_str()
                .unwrap_or("call ledger is unavailable"),
        )
        .with_detail(json!({"retryable": refusal.data["retryable"]})));
    }
    let unavailable = |message: String| {
        ErrorBody::new("database_unavailable", message).with_detail(json!({"retryable": false}))
    };
    let db = ctx
        .db()
        .ok_or_else(|| unavailable("call ledger is unavailable".into()))?;
    let incarnation = ledger::provider_incarnation();
    let now = crate::db::call_ledger::now_ms();
    match request {
        Custody::Withdraw {
            key,
            owner_as_carrier,
        } => {
            let found = {
                let conn = db
                    .lock()
                    .map_err(|_| unavailable("call ledger database mutex is poisoned".into()))?;
                ledger::withdraw(&conn, &key, stamp, now, ROLE_REPLY_BODY_LIMIT)
                    .map_err(|error| unavailable(error.to_string()))?
            };
            match found {
                Withdrawal::UnknownCall if owner_as_carrier => {
                    Err(CallerProblem::CarrierRequired.error())
                }
                Withdrawal::UnknownCall => Ok(CustodyReply {
                    body: serde_json::to_vec(&WithdrawAnswer::UnknownCall.encode())
                        .expect("withdraw answers are JSON"),
                    withdrawn_key: None,
                    kill_elsewhere: None,
                }),
                Withdrawal::NotPermitted => Err(CallerProblem::NotPermitted.error()),
                Withdrawal::Answer {
                    body,
                    kill_task,
                    held_withdrawn,
                } => {
                    let withdrawn_key = held_withdrawn.then_some(key.clone());
                    let Some(task) = kill_task else {
                        return Ok(CustodyReply {
                            body,
                            withdrawn_key,
                            kill_elsewhere: None,
                        });
                    };
                    let kill = WithdrawKill { key, task, body };
                    // A codemode program's shell calls run on its head's root,
                    // which may differ from the root its own route is bound
                    // to; their tasks are then in the head root's registry.
                    if !ctx.bash_background().holds_task(&kill.task) {
                        return Ok(CustodyReply {
                            body: Vec::new(),
                            withdrawn_key,
                            kill_elsewhere: Some(kill),
                        });
                    }
                    Ok(CustodyReply {
                        body: kill_withdrawn_call(ctx, &kill)?,
                        withdrawn_key,
                        kill_elsewhere: None,
                    })
                }
            }
        }
        Custody::LateResults(request) => {
            let conn = db
                .lock()
                .map_err(|_| unavailable("call ledger database mutex is poisoned".into()))?;
            ledger::late_results(
                &conn,
                custodian,
                incarnation,
                request.since.as_ref(),
                request.limit,
                now,
                ROLE_REPLY_BODY_LIMIT,
            )
            .map_err(|error| unavailable(error.to_string()))?
            .map(|body| CustodyReply {
                body,
                withdrawn_key: None,
                kill_elsewhere: None,
            })
        }
        Custody::Ack(request) => {
            let conn = db
                .lock()
                .map_err(|_| unavailable("call ledger database mutex is poisoned".into()))?;
            ledger::ack_through(&conn, custodian, incarnation, &request.through)
                .map_err(|error| unavailable(error.to_string()))?
                .map(|_| CustodyReply {
                    body: b"{}".to_vec(),
                    withdrawn_key: None,
                    kill_elsewhere: None,
                })
        }
    }
}

/// Runs a withdraw's owed kill on the project root whose registry holds the
/// task, then stores and returns the answer. When no bound root holds it, the
/// custody root checks the task's record instead (see [`kill_withdrawn_call`]).
async fn kill_on_owning_root(
    executor: &std::sync::Arc<crate::executor::Executor>,
    custody_root: crate::path_identity::ProjectRootId,
    kill: WithdrawKill,
    op: &str,
) -> Result<Vec<u8>, ErrorBody> {
    use super::*;
    let root = executor
        .actor_entries()
        .into_iter()
        .find(|(_, ctx)| ctx.bash_background().holds_task(&kill.task))
        .map_or(custody_root, |(root, _)| root);
    let request_id = format!("{op}-kill-{}", kill.task);
    let (result_tx, result_rx) = oneshot::channel();
    let rx = executor.submit_async(
        root,
        Lane::Mutating,
        request_id.clone(),
        Box::new(move |ctx| {
            let _ = result_tx.send(kill_withdrawn_call(ctx, &kill));
            Response::success(request_id, json!({}))
        }),
    );
    let response = await_executor_response(rx, op.to_owned()).await;
    result_rx.await.unwrap_or_else(|_| {
        Err(
            ErrorBody::new("executor_failure", format!("{op} kill did not complete"))
                .with_detail(response.data),
        )
    })
}

impl LedgerEdge {
    #[cfg(test)]
    pub(super) fn retain_shell_for_egress_test(
        &self,
        root: crate::path_identity::ProjectRootId,
        route: super::RouteChannel,
        corr: u64,
    ) {
        self.calls.lock().unwrap().insert(
            (route, corr),
            LedgerCall {
                root,
                key: crate::db::call_ledger::Key {
                    carrier: "egress-test".into(),
                    call_key: "held-detach".into(),
                },
                shell: true,
                detached: false,
                expires_at: std::time::Instant::now() + std::time::Duration::from_secs(60),
                withdrawn: None,
            },
        );
    }
    pub(super) fn new() -> (
        std::sync::Arc<Self>,
        tokio::sync::mpsc::UnboundedReceiver<super::DecodedFrame>,
    ) {
        let _ = crate::db::call_ledger::provider_incarnation();
        let (ready, receiver) = tokio::sync::mpsc::unbounded_channel();
        (
            std::sync::Arc::new(Self {
                calls: Default::default(),
                waiting: Default::default(),
                recent_sweeps: Default::default(),
                authorizing: Default::default(),
                ready,
            }),
            receiver,
        )
    }

    fn sweep_due(
        &self,
        root: &crate::path_identity::ProjectRootId,
        now: std::time::Instant,
    ) -> bool {
        let mut recent = self.recent_sweeps.lock().unwrap();
        recent.retain(|_, last| now.saturating_duration_since(*last) < super::DRAIN_TICK_PERIOD);
        if recent.contains_key(root) {
            return false;
        }
        recent.insert(root.clone(), now);
        true
    }

    pub(super) fn key(
        identity: &super::RouteIdentity,
        call_key: &str,
    ) -> crate::db::call_ledger::Key {
        // A codemode call runs with its head agent's principal, but its
        // ledger rows belong to the run route that sent it.
        let principal = identity
            .run_owner
            .as_ref()
            .map_or(&identity.spawn_principal, |owner| &owner.run_principal);
        let carrier = if super::occipital_route(identity) {
            identity
                .scope
                .as_ref()
                .map(|scope| {
                    format!(
                        "reserved:occipital#{}",
                        serde_json::to_string(&json!([
                            super::principal_id(&Some(scope.owner.clone())).unwrap(),
                            scope.scope_ref,
                            scope.scope_epoch.to_string()
                        ]))
                        .expect("scope tuple serializes")
                    )
                })
                .unwrap_or_else(|| ledger_carrier(principal))
        } else {
            ledger_carrier(principal)
        };
        crate::db::call_ledger::Key {
            carrier,
            call_key: call_key.into(),
        }
    }

    /// Returns the frame unchanged for keyless, legacy and refused calls.
    /// A refused admission never consumes a key or writes a row.
    pub(super) fn defer(
        self: &std::sync::Arc<Self>,
        decoded: super::DecodedFrame,
        routes: &std::collections::HashMap<super::RouteChannel, super::RouteIdentity>,
        executor: &std::sync::Arc<crate::executor::Executor>,
        writer: &super::WriterSender,
        metrics: &std::sync::Arc<super::DispatchPathMetrics>,
    ) -> Result<(), super::DecodedFrame> {
        use super::*;
        use crate::db::call_ledger::{self as ledger, Admission, State};
        let route = route_key(decoded.frame.header.channel, decoded.frame.header.epoch);
        let Some(identity) = routes
            .get(&route)
            .filter(|i| i.role == RouteRole::ToolProviderV1)
            .cloned()
        else {
            return Err(decoded);
        };
        let Ok(body) = decoded.body() else {
            return Err(decoded);
        };
        if body.get("op").is_none() {
            if let Some(op) = body
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| custody_operation(name))
            {
                let op = op.to_owned();
                let body = body.clone();
                self.custody(decoded, &op, &body, identity, executor, writer, metrics);
                return Ok(());
            }
        }
        if body
            .get("op")
            .and_then(Value::as_str)
            .is_some_and(|op| op != "tool.call")
            || body
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(recognized_operation)
        {
            return Err(decoded);
        }
        let Ok(call) = serde_json::from_value::<cortexkit_role_tool_provider::call::ToolCallRequest>(
            copy_call_json(body, 0),
        ) else {
            return Err(decoded);
        };
        let Some(call_key) = call.call_key.as_deref() else {
            return Err(decoded);
        };
        let carrier = super::occipital_carrier_route(&identity);
        let admitted = if carrier {
            super::carrier_read_only::admit(&identity, body).and_then(|()| {
                super::carrier_read_only::admit_operation(&identity, &call.name, &call.arguments)
                    .map(|_| ())
            })
        } else if super::occipital_route(&identity) {
            return Err(decoded);
        } else {
            admit(
                &call,
                identity.scope.is_some(),
                &identity.disabled_tools,
                crate::bash_background::powershell_available(),
                &identity.session,
                admission_trusted(&identity, &call),
            )
            .and_then(|_| super::rich_ui::admission::reject_non_carrier(&call.arguments))
        };
        if admitted.is_err() {
            return Err(decoded);
        };
        let held = matches!(identity.trust, BindTrust::Untrusted)
            && matches!(call.name.as_str(), "bash" | "powershell");
        if held {
            let mut plan_args = copy_call_json(&call.arguments, 1);
            if !identity.consumer_elicitation_capable
                || plan_args.get("sandbox").and_then(Value::as_str) == Some("host")
            {
                return Err(decoded);
            };
            if call.name == "powershell" {
                plan_args
                    .as_object_mut()
                    .unwrap()
                    .insert("shell".into(), json!("powershell"));
            }
            if bash::prepare_bash_elicitation_plan(&plan_args, &identity.project_root).is_err() {
                return Err(decoded);
            };
        }
        let key = Self::key(&identity, call_key);
        if let Some(response) = executor
            .actor_context(&identity.root)
            .and_then(|ctx| ctx.database_runtime_refusal("ledger-admission", "bash"))
        {
            let writer = writer.clone();
            let metrics = metrics.clone();
            tokio::spawn(async move {
                let result = ledger_refusal_result(&call, response, &identity.project_root);
                let outgoing = if carrier {
                    super::wire::build_carrier_response_frame(
                        &decoded.frame,
                        None,
                        super::rich_ui::carrier::error_body(
                            result.response.data["code"].as_str().unwrap_or("unknown"),
                        ),
                    )
                } else {
                    build_tool_response_frame(
                        decoded.frame.header.ver,
                        route,
                        decoded.frame.header.corr,
                        decoded.frame.header.flags,
                        &result,
                        identity.reply_policy(),
                    )
                };
                if let Ok(frame) = outgoing {
                    let _ = send_reliable_writer_frame(&writer, &metrics, frame, "ledger refusal")
                        .await;
                }
            });
            return Ok(());
        }
        // Carrier observations must not enter the scope owner's ack sequence,
        // late-results feed or retention accounting.
        let scope = if super::occipital_route(&identity) {
            None
        } else {
            identity.scope.as_ref().map(ledger_scope)
        };
        // composition_digest is the role crate's SHA-256 over JCS, without
        // schema-description projection. Hash before server shell injections.
        let digest = composition_digest(
            &json!({"name":call.name,"schema_pin":call.schema_pin,"arguments":call.arguments}),
        )
        .expect("admitted JSON has canonical bytes");
        let edge = self.clone();
        let repeat_executor = executor.clone();
        let writer = writer.clone();
        let metrics = metrics.clone();
        let root = identity.root.clone();
        let request_id = format!("ledger-{}-{}", route.channel, decoded.frame.header.corr);
        let include_tool_result_body = identity.tool_result_body && !carrier;
        let response_key = key.clone();
        let cancel = PersistentCancelSignal::new();
        let terminal_claim = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let admission = AdmissionWait::from_decoded(&decoded, &call.name);
        self.waiting.lock().unwrap().insert(
            (route, decoded.frame.header.corr),
            LedgerWait {
                admission,
                cancel: cancel.clone(),
                terminal_claim: terminal_claim.clone(),
            },
        );
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let sweep_due = self.sweep_due(&root, std::time::Instant::now());
        let rx = executor.submit_async(
            root.clone(),
            crate::executor::Lane::Mutating,
            request_id.clone(),
            Box::new(move |ctx| {
                let result = (|| {
                    // Use the database-dependent gate for every keyed tool, even a
                    // read. Keyless reads continue through their existing gate.
                    if let Some(refusal) = ctx.database_runtime_refusal(&request_id, "bash") {
                        return Err(refusal);
                    }
                    let Some(db) = ctx.db() else {
                        return Err(crate::protocol::Response::error_with_data(
                            &request_id,
                            "database_unavailable",
                            "Call ledger is unavailable; no tool operation was performed.",
                            json!({"retryable":false}),
                        ));
                    };
                    let conn = db.lock().map_err(|_| {
                        crate::protocol::Response::error(
                            &request_id,
                            "database_unavailable",
                            "Call ledger database mutex is poisoned",
                        )
                    })?;
                    let now = ledger::now_ms();
                    if sweep_due {
                        ledger::sweep(&conn, now).map_err(|error| {
                            Response::error_with_data(
                                &request_id,
                                "database_unavailable",
                                error.to_string(),
                                json!({"retryable":false}),
                            )
                        })?;
                    }
                    ledger::reduce_expired(&conn, &key, now)
                        .and_then(|_| {
                            ledger::admit_with_tool_result_source(
                                &conn,
                                &key,
                                &digest,
                                scope.as_ref(),
                                if held {
                                    State::Prepared
                                } else {
                                    State::DispatchStarted
                                },
                                include_tool_result_body,
                            )
                        })
                        .map_err(|error| {
                            crate::protocol::Response::error_with_data(
                                &request_id,
                                "database_unavailable",
                                error.to_string(),
                                json!({"retryable":false}),
                            )
                        })
                })();
                let _ = result_tx.send(result);
                crate::protocol::Response::success(request_id, json!({}))
            }),
        );
        tokio::spawn(async move {
            let response = await_executor_response(rx, "ledger-admission".into()).await;
            let result = result_rx.await.unwrap_or(Err(response));
            match result {
                Ok(Admission::New) => {
                    let admitted = {
                        let waiting = edge.waiting.lock().unwrap();
                        let admitted = waiting.contains_key(&(route, decoded.frame.header.corr));
                        if admitted {
                            edge.calls.lock().unwrap().insert(
                                (route, decoded.frame.header.corr),
                                LedgerCall {
                                    root: root.clone(),
                                    key: response_key.clone(),
                                    expires_at: std::time::Instant::now()
                                        + bash_elicitation_timeout(),
                                    withdrawn: None,
                                    shell: matches!(call.name.as_str(), "bash" | "powershell"),
                                    detached: false,
                                },
                            );
                        }
                        admitted
                    };
                    if !admitted {
                        // Drain answered before admission completed. No tool was
                        // dispatched, so leave a durable not-started result for repeats.
                        drop(repeat_executor.submit_async(root, Lane::Mutating, "ledger-drained-admission".into(), Box::new(move |ctx| {
                            if let Some(db) = ctx.db() {
                                if let Ok(conn) = db.lock() {
                                    let recorded = ledger::not_started(&response_key, "cancelled");
                                    if let Err(error) = ledger::settle(&conn, &response_key, &recorded, "not_started", Some("cancelled"), ledger::now_ms()) {
                                        log::warn!("call ledger drained admission settlement failed: {error}");
                                    }
                                }
                            }
                            Response::success("ledger-drained-admission", json!({}))
                        })));
                        return;
                    }
                    // Without `test-timing-hooks` the gate is an empty async fn:
                    // it completes on its first poll and never yields, so the
                    // admission check above and the send stay one step on the
                    // frame loop's single-threaded runtime.
                    admission_gate().await;
                    let _ = edge.ready.send(decoded);
                }
                Ok(Admission::Repeat(row)) if row.state != State::Settled => {
                    // Repeats observe the same durable execution, including a
                    // shell recovered after restart. They never dispatch again.
                    loop {
                        if terminal_claim.load(Ordering::SeqCst) {
                            break;
                        }
                        if cancel.is_cancelled() {
                            if terminal_claim.swap(true, Ordering::SeqCst) {
                                break;
                            }
                            let frame = Self::cancelled_frame(&decoded.frame).unwrap();
                            edge.finish_waiting(&decoded.frame);
                            let _ = send_reliable_writer_frame(
                                &writer,
                                &metrics,
                                frame,
                                "cancelled ledger attachment",
                            )
                            .await;
                            break;
                        }
                        let key = response_key.clone();
                        let (row_tx, row_rx) = oneshot::channel();
                        let rx = repeat_executor.submit_async(
                            root.clone(),
                            Lane::Mutating,
                            "ledger-attach".into(),
                            Box::new(move |ctx| {
                                let row = ctx.db().and_then(|db| {
                                    db.lock().ok().and_then(|conn| {
                                        if include_tool_result_body {
                                            ledger::get_for_replay(&conn, &key)
                                        } else {
                                            ledger::get(&conn, &key)
                                        }
                                        .ok()
                                        .flatten()
                                    })
                                });
                                let _ = row_tx.send(row);
                                Response::success("ledger-attach", json!({}))
                            }),
                        );
                        tokio::select! {
                            _ = await_executor_response(rx, "ledger-attach".into()) => {},
                            _ = cancel.cancelled() => continue,
                        }
                        if let Ok(Some(row)) = row_rx.await {
                            if row.state == State::Settled {
                                if terminal_claim.swap(true, Ordering::SeqCst) {
                                    break;
                                }
                                if let Ok(frame) = edge.finish_recorded_result(
                                    &decoded.frame,
                                    &response_key,
                                    row.frame.as_ref(),
                                ) {
                                    let _ = send_reliable_writer_frame(
                                        &writer,
                                        &metrics,
                                        if carrier {
                                            WriterFrame::plain(frame)
                                        } else {
                                            WriterFrame::retained_tool_response_with_source(
                                                frame,
                                                identity.reply_policy(),
                                                call.arguments.get("path").and_then(Value::as_str),
                                                row.tool_result_source,
                                            )
                                        },
                                        "attached ledger result",
                                    )
                                    .await;
                                }
                                break;
                            }
                        } else {
                            if terminal_claim.swap(true, Ordering::SeqCst) {
                                break;
                            }
                            edge.finish_waiting(&decoded.frame);
                            let _ = send_provider_error(
                                &writer,
                                &metrics,
                                &decoded.frame,
                                ErrorBody::new(
                                    "database_unavailable",
                                    "call ledger attachment is unavailable",
                                )
                                .with_detail(json!({"retryable":false})),
                            )
                            .await;
                            break;
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(250)) => {},
                            _ = cancel.cancelled() => {},
                        }
                    }
                }
                Ok(Admission::Repeat(row)) => {
                    if terminal_claim.swap(true, Ordering::SeqCst) {
                        return;
                    }
                    if let Ok(frame) = edge.finish_recorded_result(
                        &decoded.frame,
                        &response_key,
                        row.frame.as_ref(),
                    ) {
                        let _ = send_reliable_writer_frame(
                            &writer,
                            &metrics,
                            if carrier {
                                WriterFrame::plain(frame)
                            } else {
                                WriterFrame::retained_tool_response_with_source(
                                    frame,
                                    identity.reply_policy(),
                                    call.arguments.get("path").and_then(Value::as_str),
                                    row.tool_result_source,
                                )
                            },
                            "ledger replay",
                        )
                        .await;
                    }
                }
                Ok(Admission::Conflict) => {
                    if terminal_claim.swap(true, Ordering::SeqCst) {
                        return;
                    }
                    let error = errors::invalid_request(
                        "call_key",
                        "key already names different content or scope",
                    );
                    let recorded = ledger::RecordedFrame {
                        ty: FrameType::Error,
                        body: serde_json::to_vec(&error).unwrap(),
                    };
                    if let Ok(frame) = edge.finish_replay(&decoded.frame, &recorded) {
                        let _ =
                            send_reliable_writer_frame(&writer, &metrics, frame, "ledger conflict")
                                .await;
                    }
                }
                Err(response) => {
                    if terminal_claim.swap(true, Ordering::SeqCst) {
                        return;
                    }
                    let result = ledger_refusal_result(&call, response, &identity.project_root);
                    let outgoing = if carrier {
                        super::wire::build_carrier_response_frame(
                            &decoded.frame,
                            None,
                            super::rich_ui::carrier::error_body(
                                result.response.data["code"].as_str().unwrap_or("unknown"),
                            ),
                        )
                    } else {
                        build_tool_response_frame(
                            decoded.frame.header.ver,
                            route,
                            decoded.frame.header.corr,
                            decoded.frame.header.flags,
                            &result,
                            identity.reply_policy(),
                        )
                    };
                    if let Ok(mut frame) = outgoing {
                        if edge.finish_waiting(&decoded.frame) {
                            frame = Self::cancelled_frame(&decoded.frame).unwrap().into();
                        }
                        let _ =
                            send_reliable_writer_frame(&writer, &metrics, frame, "ledger refusal")
                                .await;
                    }
                }
            }
        });
        Ok(())
    }

    fn replay(
        request: &subc_protocol::Frame,
        recorded: &crate::db::call_ledger::RecordedFrame,
    ) -> Result<subc_protocol::Frame, subc_protocol::FrameBuildError> {
        subc_protocol::Frame::build_with_version(
            request.header.ver,
            recorded.ty,
            request.header.flags,
            request.header.channel,
            request.header.epoch,
            request.header.corr,
            recorded.body.clone(),
        )
    }

    /// Answers a custody request (`tool.withdraw`, `late_results`,
    /// `late_results.ack`) on a v1 route. The caller checks run here; the
    /// ledger read and any settlement run as one executor job on the route
    /// root's database connection, never on the frame loop. A withdraw that
    /// settles a held call also asks the frame loop, through the ready
    /// channel, to end that call's request with `not_started`.
    #[allow(clippy::too_many_arguments)]
    fn custody(
        self: &std::sync::Arc<Self>,
        decoded: super::DecodedFrame,
        op: &str,
        body: &Value,
        identity: super::RouteIdentity,
        executor: &std::sync::Arc<crate::executor::Executor>,
        writer: &super::WriterSender,
        metrics: &std::sync::Arc<super::DispatchPathMetrics>,
    ) {
        use super::*;
        let request = decoded.frame;
        let writer = writer.clone();
        let metrics = metrics.clone();
        let custody = match custody_request(op, body, &identity) {
            Ok(custody) => custody,
            Err(error) => {
                tokio::spawn(async move {
                    let _ = send_provider_error(&writer, &metrics, &request, error).await;
                });
                return;
            }
        };
        let custodian = route_principal(&identity);
        let stamp = identity.scope.as_ref().map(ledger_scope);
        let request_id = format!("{op}-{}-{}", request.header.channel, request.header.corr);
        let (result_tx, result_rx) = oneshot::channel();
        let rx = executor.submit_async(
            identity.root.clone(),
            Lane::Mutating,
            request_id.clone(),
            Box::new(move |ctx| {
                let _ = result_tx.send(run_custody(
                    ctx,
                    custody,
                    &custodian,
                    stamp.as_ref(),
                    &request_id,
                ));
                Response::success(request_id, json!({}))
            }),
        );
        let edge = self.clone();
        let op = op.to_owned();
        let executor = executor.clone();
        let custody_root = identity.root.clone();
        tokio::spawn(async move {
            let response = await_executor_response(rx, op.clone()).await;
            let result = result_rx.await.unwrap_or_else(|_| {
                Err(
                    ErrorBody::new("executor_failure", format!("{op} did not complete"))
                        .with_detail(response.data),
                )
            });
            let result = match result {
                Ok(CustodyReply {
                    kill_elsewhere: Some(kill),
                    withdrawn_key,
                    ..
                }) => kill_on_owning_root(&executor, custody_root, kill, &op)
                    .await
                    .map(|body| CustodyReply {
                        body,
                        withdrawn_key,
                        kill_elsewhere: None,
                    }),
                other => other,
            };
            match result {
                Ok(reply) => {
                    if let Some(key) = reply.withdrawn_key {
                        edge.end_withdrawn_held_calls(&key, "withdrawn");
                    }
                    if let Ok(frame) = subc_protocol::Frame::build_with_version(
                        request.header.ver,
                        FrameType::Response,
                        request.header.flags,
                        request.header.channel,
                        request.header.epoch,
                        request.header.corr,
                        reply.body,
                    ) {
                        let _ =
                            send_reliable_writer_frame(&writer, &metrics, frame, "custody reply")
                                .await;
                    }
                }
                Err(error) => {
                    let _ = send_provider_error(&writer, &metrics, &request, error).await;
                }
            }
        });
    }

    /// Hands the frame loop one `Cancel`-typed frame per live request of the
    /// withdrawn call. The frame loop removes the call's pending question
    /// and answers the request `not_started`; a request whose question is not
    /// filed yet is marked, and answered when the frame loop reaches it.
    fn end_withdrawn_held_calls(&self, key: &crate::db::call_ledger::Key, reason: &'static str) {
        let requests: Vec<_> = self
            .calls
            .lock()
            .unwrap()
            .iter_mut()
            .filter(|(_, call)| &call.key == key)
            .map(|(request, call)| {
                call.withdrawn = Some(reason);
                *request
            })
            .collect();
        for (route, corr) in requests {
            match subc_protocol::Frame::build(
                subc_protocol::FrameType::Cancel,
                super::control_flags(),
                route.channel,
                route.epoch,
                corr,
                Vec::new(),
            ) {
                Ok(frame) => {
                    let _ = self.ready.send(super::DecodedFrame {
                        frame,
                        phase_trace: crate::run_tool_call::PhaseTrace::new(
                            std::time::Instant::now(),
                        ),
                        parsed_body: Default::default(),
                        confinement: None,
                        launch_restore: None,
                        codemode_policies: None,
                    });
                }
                Err(error) => log::warn!("withdrawn call could not be ended: {error}"),
            }
        }
    }

    /// Remove permission questions as tool.withdraw does, but preserve
    /// scope_ended: the scope ended, not the caller's decision to withdraw.
    pub(super) fn scope_ended(&self, keys: &[crate::db::call_ledger::Key]) {
        for key in keys {
            self.end_withdrawn_held_calls(key, "scope_ended");
        }
    }

    /// The shell command outlives the request on its former route. Once that
    /// request cannot receive a reply, terminal task writes must store the
    /// result in the call ledger for replay and late_results.
    pub(super) async fn detach_shells(
        &self,
        executor: &std::sync::Arc<crate::executor::Executor>,
        route: Option<super::RouteChannel>,
    ) {
        let calls: Vec<_> = self
            .calls
            .lock()
            .unwrap()
            .iter_mut()
            .filter(|((channel, _), call)| {
                call.shell && !call.detached && route.is_none_or(|route| *channel == route)
            })
            .map(|(_, call)| {
                call.detached = true;
                (call.root.clone(), call.key.clone())
            })
            .collect();
        for (root, key) in calls {
            let rx = executor.submit_async(
                root,
                crate::executor::Lane::Mutating,
                "ledger-detach".into(),
                Box::new(move |ctx| {
                    detach_shell_row(ctx, &key);
                    crate::protocol::Response::success("ledger-detach", serde_json::json!({}))
                }),
            );
            let _ = super::await_executor_response(rx, "ledger-detach".into()).await;
        }
    }

    pub(super) fn unrouted_shells(
        &self,
        routes: &std::collections::HashMap<super::RouteChannel, super::RouteIdentity>,
    ) -> std::collections::HashSet<super::RouteChannel> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|((route, _), call)| {
                call.shell && !call.detached && !routes.contains_key(route)
            })
            .map(|((route, _), _)| *route)
            .collect()
    }

    /// Marks a held request withdrawn before its question was filed. Returns
    /// false when the request already ended.
    pub(super) fn mark_withdrawn(&self, route: super::RouteChannel, corr: u64) -> bool {
        match self.calls.lock().unwrap().get_mut(&(route, corr)) {
            Some(call) => {
                call.withdrawn.get_or_insert("withdrawn");
                true
            }
            None => false,
        }
    }

    /// Whether a `tool.withdraw` settled this admitted request's call.
    pub(super) fn is_withdrawn(&self, request: &subc_protocol::Frame) -> bool {
        self.calls
            .lock()
            .unwrap()
            .get(&(
                super::route_key(request.header.channel, request.header.epoch),
                request.header.corr,
            ))
            .is_some_and(|call| call.withdrawn.is_some())
    }

    /// The terminal frame of a held request whose call was withdrawn before
    /// it dispatched, byte-identical to the frame recorded on its row.
    pub(super) fn withdrawn_frame(
        &self,
        route: super::RouteChannel,
        corr: u64,
        ver: u8,
        flags: subc_protocol::Flags,
    ) -> Option<Result<subc_protocol::Frame, subc_protocol::FrameBuildError>> {
        let (key, reason) = {
            let calls = self.calls.lock().unwrap();
            let call = calls.get(&(route, corr))?;
            (call.key.clone(), call.withdrawn.unwrap_or("withdrawn"))
        };
        let recorded = crate::db::call_ledger::not_started(&key, reason);
        Some(subc_protocol::Frame::build_with_version(
            ver,
            recorded.ty,
            flags,
            route.channel,
            route.epoch,
            corr,
            recorded.body,
        ))
    }

    /// Pending admission and attached repeats do not belong to the tool's
    /// executor job. Cancelling one attachment must not settle the shared row.
    pub(super) fn cancel(&self, route: super::RouteChannel, corr: u64) -> bool {
        let waiting = self.waiting.lock().unwrap();
        if let Some(signal) = waiting.get(&(route, corr)) {
            signal.cancel.cancel();
            true
        } else {
            false
        }
    }

    pub(super) fn close(&self) {
        for signal in self.waiting.lock().unwrap().values() {
            signal.cancel.cancel();
        }
    }

    pub(super) fn has_waiting(&self, request: &subc_protocol::Frame) -> bool {
        self.waiting.lock().unwrap().contains_key(&(
            super::route_key(request.header.channel, request.header.epoch),
            request.header.corr,
        ))
    }

    pub(super) fn held_since(&self) -> Vec<std::time::Instant> {
        self.waiting
            .lock()
            .unwrap()
            .values()
            .map(|wait| wait.admission.held_since)
            .collect()
    }

    pub(super) fn drain_for_module_drain(
        &self,
    ) -> Result<Vec<subc_protocol::Frame>, super::SubcError> {
        let mut frames = Vec::new();
        for (_, wait) in self.waiting.lock().unwrap().drain() {
            if !wait
                .terminal_claim
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                wait.cancel.cancel();
                frames.push(wait.admission.draining_frame()?);
            }
        }
        Ok(frames)
    }

    pub(super) fn finish_waiting(&self, request: &subc_protocol::Frame) -> bool {
        self.waiting
            .lock()
            .unwrap()
            .remove(&(
                super::route_key(request.header.channel, request.header.epoch),
                request.header.corr,
            ))
            .is_some_and(|waiting| waiting.cancel.is_cancelled())
    }

    fn cancelled_frame(
        request: &subc_protocol::Frame,
    ) -> Result<subc_protocol::Frame, super::SubcError> {
        super::build_error_frame(
            request.header.ver,
            request.header.channel,
            request.header.epoch,
            request.header.corr,
            request.header.flags,
            "cancelled",
            "request cancelled",
        )
    }

    fn finish_replay(
        &self,
        request: &subc_protocol::Frame,
        recorded: &crate::db::call_ledger::RecordedFrame,
    ) -> Result<subc_protocol::Frame, super::SubcError> {
        if self.finish_waiting(request) {
            Self::cancelled_frame(request)
        } else {
            Self::replay(request, recorded).map_err(super::SubcError::FrameBuild)
        }
    }

    fn finish_recorded_result(
        &self,
        request: &subc_protocol::Frame,
        key: &crate::db::call_ledger::Key,
        recorded: Option<&crate::db::call_ledger::RecordedFrame>,
    ) -> Result<subc_protocol::Frame, super::SubcError> {
        if self.finish_waiting(request) {
            return Self::cancelled_frame(request);
        }
        if let Some(recorded) = recorded {
            match Self::replay(request, recorded) {
                Ok(frame) => return Ok(frame),
                Err(error) => log::warn!("call ledger replay could not rebuild terminal: {error}"),
            }
        } else {
            log::warn!("call ledger settled row has no retained terminal");
        }
        Self::replay(
            request,
            &crate::db::call_ledger::not_retained(key, "unknown"),
        )
        .map_err(super::SubcError::FrameBuild)
    }

    pub(super) fn authorize(
        self: &std::sync::Arc<Self>,
        decoded: super::DecodedFrame,
        asks: &std::collections::HashMap<super::ReverseCorrKey, super::PendingBashAsk>,
        executor: &std::sync::Arc<crate::executor::Executor>,
    ) -> Result<(), super::DecodedFrame> {
        use super::*;
        use crate::db::call_ledger::{self as ledger, State};
        let route = route_key(decoded.frame.header.channel, decoded.frame.header.epoch);
        if decoded.frame.header.ty != FrameType::Response
            || !bash_elicitation_reply_is_allow(&decoded.frame.body)
        {
            return Err(decoded);
        };
        let ask_key = ReverseCorrKey {
            route,
            corr: decoded.frame.header.corr,
        };
        let Some(ask) = asks.get(&ask_key) else {
            return Err(decoded);
        };
        let (root, key) = {
            let calls = self.calls.lock().unwrap();
            let Some(call) = calls.get(&(route, ask.tool_corr)) else {
                return Err(decoded);
            };
            (call.root.clone(), call.key.clone())
        };
        // Ignore duplicate allow replies while the first database update runs.
        // A duplicate's failed transition must not consume the permission entry
        // before the first update can release its authorized reply.
        if !self.authorizing.lock().unwrap().insert(ask_key) {
            return Ok(());
        }
        let edge = self.clone();
        let rx = executor.submit_async(
            root,
            Lane::Mutating,
            "ledger-authorize".into(),
            Box::new(move |ctx| {
                let authorization = (|| {
                    let db = ctx
                        .db()
                        .ok_or_else(|| "Call ledger is unavailable".to_owned())?;
                    let conn = db
                        .lock()
                        .map_err(|_| "Call ledger database mutex is poisoned".to_owned())?;
                    let authorized =
                        ledger::transition(&conn, &key, State::Prepared, State::Authorized)
                            .map_err(|error| error.to_string())?;
                    if !authorized {
                        return Ok(false);
                    }
                    ledger::transition(&conn, &key, State::Authorized, State::DispatchStarted)
                        .map_err(|error| error.to_string())
                })();
                match authorization {
                    Ok(true) => Response::success("ledger-authorize", json!({"allowed":true})),
                    result => Response::error(
                        "ledger-authorize",
                        "database_unavailable",
                        match result {
                            Err(error) => error,
                            _ => "Durable authorization transition was not applied".to_owned(),
                        },
                    ),
                }
            }),
        );
        tokio::spawn(async move {
            let response = await_executor_response(rx, "ledger-authorize".into()).await;
            let mut decoded = decoded;
            if response.data["allowed"] != true {
                log::warn!(
                    "call ledger authorization failed channel={} epoch={} corr={}: {}",
                    route.channel,
                    route.epoch,
                    ask_key.corr,
                    response.data
                );
                decoded.frame = build_error_frame(
                    decoded.frame.header.ver,
                    route.channel,
                    route.epoch,
                    ask_key.corr,
                    decoded.frame.header.flags,
                    "database_unavailable",
                    AUTHORIZATION_FAILURE_MESSAGE,
                )
                .expect("bounded authorization failure uses a valid ask header");
                decoded.parsed_body = Default::default();
            }
            let _ = edge.ready.send(decoded);
            edge.authorizing.lock().unwrap().remove(&ask_key);
        });
        Ok(())
    }

    pub(super) fn proxy(
        self: &std::sync::Arc<Self>,
        actual: super::WriterSender,
        executor: std::sync::Arc<crate::executor::Executor>,
        _metrics: std::sync::Arc<super::DispatchPathMetrics>,
    ) -> (super::WriterSender, tokio::task::JoinHandle<()>) {
        use super::*;
        use crate::db::call_ledger::{self as ledger, RecordedFrame, State};
        let (sender, mut receiver) = mpsc::channel::<WriterFrame>(WRITER_QUEUE_CAPACITY);
        let edge = self.clone();
        let task = tokio::spawn(async move {
            while let Some(mut outgoing) = receiver.recv().await {
                let route = route_key(outgoing.header.channel, outgoing.header.epoch);
                let terminal = matches!(
                    outgoing.header.ty,
                    FrameType::Response | FrameType::Error | FrameType::StreamEnd
                );
                let call = if terminal {
                    edge.calls
                        .lock()
                        .unwrap()
                        .remove(&(route, outgoing.header.corr))
                } else {
                    None
                };
                let Some(call) = call else {
                    if actual.send(outgoing).await.is_err() {
                        break;
                    }
                    continue;
                };
                if outgoing.header.ty == FrameType::Error
                    && serde_json::from_slice::<Value>(&outgoing.body)
                        .is_ok_and(|body| body["code"] == "detached")
                {
                    // ERROR detached ends the waiting request, not the running
                    // command. A terminal task write will record its real result.
                    let rx = executor.submit_async(
                        call.root,
                        Lane::Mutating,
                        "ledger-detach".into(),
                        Box::new(move |ctx| {
                            detach_shell_row(ctx, &call.key);
                            Response::success("ledger-detach", json!({}))
                        }),
                    );
                    let _ = await_executor_response(rx, "ledger-detach".into()).await;
                    if actual.send(outgoing).await.is_err() {
                        break;
                    }
                    continue;
                }
                let actual = actual.clone();
                let (tool_body_for_job, projection_error) =
                    match outgoing.tool_result_source_for_retention() {
                        Ok(body) => (body, None),
                        Err(error) => (None, Some(error.to_string())),
                    };
                let recorded = RecordedFrame {
                    ty: outgoing.header.ty,
                    body: copy_terminal_body(&outgoing.body),
                };
                let record_for_job = recorded;
                let rx = executor.submit_async(
                    call.root.clone(),
                    Lane::Mutating,
                    "ledger-settle".into(),
                    Box::new(move |ctx| {
                        if let Some(error) = projection_error {
                            return Response::error("ledger-settle", "database_unavailable", error);
                        }
                        let Some(db) = ctx.db() else {
                            return Response::error(
                                "ledger-settle",
                                "database_unavailable",
                                "Call ledger is unavailable",
                            );
                        };
                        let Ok(conn) = db.lock() else {
                            return Response::error(
                                "ledger-settle",
                                "database_unavailable",
                                "Call ledger database mutex is poisoned",
                            );
                        };
                        let row = ledger::get(&conn, &call.key).ok().flatten();
                        let reason =
                            row.as_ref()
                                .filter(|row| row.state == State::Prepared)
                                .map(|_| {
                                    if std::time::Instant::now() >= call.expires_at {
                                        "expired"
                                    } else {
                                        "denied"
                                    }
                                });
                        let inferred = ledger::outcome(&record_for_job);
                        let outcome = if reason.is_some() {
                            "bash_denied_untrusted".into()
                        } else if record_for_job.ty == FrameType::Response && inferred == "unknown"
                        {
                            row.and_then(|row| row.outcome)
                                .filter(|outcome| outcome != "ok")
                                .unwrap_or(inferred)
                        } else {
                            inferred
                        };
                        match ledger::settle_with_tool_result_source(
                            &conn,
                            &call.key,
                            &record_for_job,
                            &outcome,
                            reason,
                            ledger::now_ms(),
                            tool_body_for_job.as_deref(),
                        ) {
                            Ok(true) => {}
                            Ok(false)
                                if ledger::get_for_replay(&conn, &call.key)
                                    .ok()
                                    .flatten()
                                    .is_some_and(|row| {
                                        row.tool_result_source.as_deref()
                                            == tool_body_for_job.as_deref()
                                            && row.frame.is_some_and(|frame| {
                                                frame.ty == record_for_job.ty
                                                    && frame.body == record_for_job.body
                                            })
                                    }) => {}
                            result => {
                                return Response::error(
                                    "ledger-settle",
                                    "database_unavailable",
                                    match result {
                                        Err(error) => error.to_string(),
                                        _ => "Call ledger did not retain this terminal result"
                                            .to_owned(),
                                    },
                                )
                            }
                        }
                        Response::success("ledger-settle", json!({}))
                    }),
                );
                tokio::spawn(async move {
                    let settled = await_executor_response(rx, "ledger-settle".into()).await;
                    if !settled.success {
                        log::warn!(
                            "call ledger settlement failed channel={} epoch={} corr={}: {}",
                            outgoing.header.channel,
                            outgoing.header.epoch,
                            outgoing.header.corr,
                            settled.data
                        );
                        outgoing.frame = uncertain_settlement_frame(&outgoing.frame);
                    }
                    if actual.send(outgoing).await.is_err() {
                        return;
                    }
                });
            }
        });
        (sender, task)
    }
}

fn detach_shell_row(ctx: &crate::context::AppContext, key: &crate::db::call_ledger::Key) {
    let task = ctx.db().and_then(|db| {
        let conn = db.lock().ok()?;
        // recovered=1 makes terminal task writes record the call's result
        // instead of waiting for an attached reply. A detach needs that same
        // behavior as restart recovery, since the command keeps running.
        if let Err(error) = conn.execute("UPDATE call_ledger SET recovered=1 WHERE carrier=?1 AND call_key=?2 AND state='DispatchStarted'", rusqlite::params![key.carrier, key.call_key]) {
            log::warn!("call ledger detach failed: {error}");
            return None;
        }
        crate::db::call_ledger::get(&conn, key).ok().flatten()?.task_id
    });
    if let Some(task) = task {
        ctx.bash_background().settle_detached_call(&task);
    }
}

pub(super) const AUTHORIZATION_FAILURE_MESSAGE: &str =
    "Authorization could not be durably recorded; the command was not started.";

const SETTLEMENT_RESULT_EXCERPT_BYTES: usize = 8192;

fn uncertain_settlement_frame(original: &subc_protocol::Frame) -> subc_protocol::Frame {
    // Untrusted routes already omit native structured fields from this wire
    // body. Excerpt that filtered body; do not rebuild it from a native response.
    let prefix_len = original.body.len().min(SETTLEMENT_RESULT_EXCERPT_BYTES);
    let prefix = String::from_utf8_lossy(&original.body[..prefix_len]);
    let end = prefix.floor_char_boundary(SETTLEMENT_RESULT_EXCERPT_BYTES);
    let truncated = prefix_len < original.body.len() || end < prefix.len();
    super::build_error_frame_with_detail(original.header.ver, original.header.channel,
        original.header.epoch, original.header.corr, original.header.flags,
        "outcome_unknown",
        "The request produced a terminal result, but AFT could not confirm that it was durably recorded. Its durable outcome is uncertain; replay is not guaranteed.",
        json!({"original_result_excerpt": &prefix[..end], "original_result_truncated": truncated}),
    ).expect("bounded settlement error uses a valid terminal header")
}

fn ledger_refusal_result(
    call: &cortexkit_role_tool_provider::call::ToolCallRequest,
    response: crate::protocol::Response,
    project_root: &std::path::Path,
) -> crate::run_tool_call::ToolCallResult {
    let context = crate::subc_format::FormatContext::from_tool_call(
        &call.name,
        &call.arguments,
        project_root,
    );
    crate::run_tool_call::ToolCallResult {
        text: crate::subc_format::format_response_with_context(&call.name, &response, &context),
        response,
    }
}

/// The tools whose file argument the catalog once advertised as `filePath`.
/// It now advertises the canonical `path`, but a runner keeps the catalog its
/// session started with, so those sessions still send `filePath`.
const LEGACY_FILE_PATH_TOOLS: [&str; 3] = ["read", "write", "edit"];

/// A call's arguments as admission checks them: for a tool in
/// [`LEGACY_FILE_PATH_TOOLS`], `filePath` is read as `path` (the command
/// itself still accepts both spellings). Sending both is refused by name
/// rather than picking one. `None` means the arguments are checked as sent.
fn legacy_file_path_arguments(
    name: &str,
    arguments: &serde_json::Map<String, Value>,
) -> Result<Option<Value>, ErrorBody> {
    if !LEGACY_FILE_PATH_TOOLS.contains(&name) || !arguments.contains_key("filePath") {
        return Ok(None);
    }
    if arguments.contains_key("path") {
        return Err(ErrorBody::new(
            errors::INVALID_REQUEST,
            "both `path` and `filePath` were sent; they name the same argument, so send only `path`",
        )
        .with_detail(json!({"field": "filePath", "conflicts_with": "path"})));
    }
    Ok(Some(Value::Object(
        arguments
            .iter()
            .map(|(key, value)| {
                let key = if key == "filePath" { "path" } else { key };
                (key.to_string(), value.clone())
            })
            .collect(),
    )))
}

/// The schema digests a [`LEGACY_FILE_PATH_TOOLS`] tool had while its catalog
/// advertised `filePath`: the served schemas with `path` renamed back. A
/// schema pin taken from that older catalog stays valid, because admission
/// accepts `filePath` and the command behaves the same.
fn legacy_file_path_digests(name: &str) -> &'static [String] {
    static DIGESTS: LazyLock<HashMap<&'static str, Vec<String>>> = LazyLock::new(|| {
        LEGACY_FILE_PATH_TOOLS
            .iter()
            .map(|name| {
                let served = served_tools(CatalogPreset::Worker)
                    .iter()
                    .find(|tool| tool.catalog.name == *name)
                    .expect("every legacy filePath tool is served");
                let digests = [Some(&served.catalog), served.with_ref.as_ref()]
                    .into_iter()
                    .flatten()
                    .map(|tool| {
                        let mut schema = tool.input_schema.clone();
                        let object = schema.as_object_mut().expect("tool schemas are objects");
                        if let Some(Value::Object(properties)) = object.get_mut("properties") {
                            *properties = std::mem::take(properties)
                                .into_iter()
                                .map(|(key, value)| {
                                    let key = if key == "path" {
                                        "filePath".into()
                                    } else {
                                        key
                                    };
                                    (key, value)
                                })
                                .collect();
                        }
                        if let Some(Value::Array(required)) = object.get_mut("required") {
                            for entry in required {
                                if entry == "path" {
                                    *entry = json!("filePath");
                                }
                            }
                        }
                        schema_digest(&schema).expect("embedded schema has a structural digest")
                    })
                    .collect();
                (*name, digests)
            })
            .collect()
    });
    DIGESTS.get(name).map_or(&[], Vec::as_slice)
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
    admit_role_name(
        &call.name,
        disabled,
        powershell_available,
        session,
        trusted,
        role,
    )?;
    admit_arguments(call, role, true)
}

/// Role admission is separate from served-argument validation because carrier
/// bindings and operation arguments are translated only after task ownership.
pub(super) fn admit_role_name(
    name: &str,
    disabled: &[String],
    powershell_available: bool,
    session: &str,
    trusted: bool,
    role: CallerRole,
) -> Result<(), ErrorBody> {
    if !METADATA
        .iter()
        .chain(PRESET_ONLY_METADATA)
        .any(|(tool, _, _)| *tool == name)
    {
        return Err(errors::unknown_tool(name));
    }
    if role == CallerRole::Reader && !READER_TOOLS.contains(&name) {
        return Err(ErrorBody::new(
            errors::UNKNOWN_TOOL,
            format!("tool {:?} is not served by the \"reader\" preset", name),
        )
        .with_detail(json!({"tool": name, "preset": CatalogPreset::Reader.name()})));
    }
    if crate::tool_gate::refusal_by_name("v1-admission", name, disabled).is_some() {
        return Err(errors::tool_disabled(name));
    }
    if name == "powershell" && !powershell_available {
        return Err(
            ErrorBody::new(errors::TOOL_UNAVAILABLE, "PowerShell is not available")
                .with_detail(json!({"tool": name, "reason": "under_review"})),
        );
    }
    if session.is_empty() {
        return Err(errors::invalid_request(
            "session",
            "v1 execution requires a bound session",
        ));
    }
    if let Some(error) = shell_capability_refusal(name, trusted) {
        return Err(error);
    }
    Ok(())
}

pub(super) fn admit_arguments(
    call: &cortexkit_role_tool_provider::call::ToolCallRequest,
    role: CallerRole,
    check_pin: bool,
) -> Result<(), ErrorBody> {
    // The worker preset serves every head tool plus the preset-only ones, so
    // its catalog holds the served schema of every admitted name.
    let served = served_tools(CatalogPreset::Worker)
        .iter()
        .find(|tool| tool.catalog.name == call.name)
        .expect("admitted tool has a schema");
    // A bash call is checked against the schema with `runon`: whether the
    // session may run remotely is decided when the call runs, by name. A
    // read, outline or zoom call is checked against the schema with `ref` the
    // same way; the command refuses `ref` while `revision_reads` is off.
    let tool = served
        .with_runon
        .as_ref()
        .or(served.with_ref.as_ref())
        .unwrap_or(&served.catalog);
    // Shared argument validation accepts both shell schemas. Check the behavior
    // version against the caller's preset so a worker wait change does not
    // invalidate calls pinned to the head's unchanged bash behavior.
    let semantics = if role.is_worker() {
        tool.semantics
    } else {
        served_tools(CatalogPreset::Head)
            .iter()
            .find(|entry| entry.catalog.name == call.name)
            .map_or(tool.semantics, |entry| entry.catalog.semantics)
    };
    if let Some(encoded) = call.schema_pin.as_ref().filter(|_| check_pin) {
        let pin = SchemaPin::parse(encoded)
            .map_err(|error| errors::invalid_request("schema_pin", error.to_string()))?;
        if pin.schema_digest != tool.schema_digest
            && pin.schema_digest != served.catalog.schema_digest
            && !legacy_file_path_digests(&call.name).contains(&pin.schema_digest)
        {
            return Err(ErrorBody::new(errors::TOOL_SCHEMA_CHANGED, "schema pin is stale").with_detail(json!({"tool": call.name, "expected": pin.schema_digest, "current": tool.schema_digest})));
        }
        if pin.semantics != semantics {
            return Err(
                ErrorBody::new(errors::TOOL_SEMANTICS_CHANGED, "semantics pin is stale")
                    .with_detail(
                        json!({"tool": call.name, "expected": pin.semantics, "current": semantics}),
                    ),
            );
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
    let legacy = if call.name == "zoom" {
        // Keep the existing refusal for mixed batch and top-level forms, even
        // though the translator can combine them into a single batch.
        if arguments.contains_key("batch") {
            if let Some(key) = arguments.keys().find(|key| !properties.contains_key(*key)) {
                return Err(errors::invalid_request(
                    key,
                    "argument is absent from the served schema",
                ));
            }
        }
        // Legacy zoom spellings are runtime aliases, not catalog properties.
        // Check a normalized copy; dispatch still translates the original call.
        let mut normalized = arguments.clone();
        crate::subc_translate::normalize_zoom_aliases(&mut normalized)
            .map_err(|error| errors::invalid_request("arguments", error.message))?;
        Some(Value::Object(normalized))
    } else {
        legacy_file_path_arguments(&call.name, arguments)?
    };
    let (arguments, checked) = match &legacy {
        Some(renamed) => (
            renamed
                .as_object()
                .expect("renamed arguments are an object"),
            renamed,
        ),
        None => (arguments, &call.arguments),
    };
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
    if let Some(error) = served.validator.iter_errors(checked).next() {
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
    #[test]
    fn catalog_modes_match_exact_tool_table_and_keep_all_classic_tools() {
        use cortexkit_role_tool_provider::{
            catalog::{check_modes, serves_call_mode},
            CallMode,
        };
        let table = [
            ("status", false),
            ("bash", false),
            ("powershell", false),
            ("read", true),
            ("write", false),
            ("edit", false),
            ("apply_patch", false),
            ("grep", true),
            ("glob", true),
            ("search", true),
            ("outline", true),
            ("zoom", true),
            ("inspect", false),
            ("callgraph", true),
            ("conflicts", false),
            ("ast_search", false),
            ("ast_replace", false),
            ("delete", false),
            ("move", false),
            ("import", false),
            ("safety", false),
            ("bash_status", false),
            ("bash_kill", false),
            ("bash_write", false),
        ];
        let head = tools(&[], true);
        assert_eq!(head.len(), table.len());
        for (name, flow) in table {
            let tool = head.iter().find(|tool| tool.name == name).unwrap();
            let expected = if flow {
                vec![CallMode::Classic, CallMode::Codemode, CallMode::Flow]
            } else {
                vec![CallMode::Classic, CallMode::Codemode]
            };
            assert_eq!(tool.modes.as_ref(), Some(&expected), "{name}");
            check_modes(tool).unwrap();
            assert!(serves_call_mode(tool, &CallMode::Classic), "{name}");
            assert!(
                !serves_call_mode(tool, &CallMode::Unknown("future".into())),
                "{name}"
            );
        }
        for preset in [CatalogPreset::Worker, CatalogPreset::Reader] {
            for tool in tools_for_session(preset, &[], true, false, false) {
                check_modes(&tool).unwrap();
                assert!(serves_call_mode(&tool, &CallMode::Classic));
                if tool.name == "bash_watch" {
                    assert_eq!(tool.modes, Some(vec![CallMode::Classic]));
                } else {
                    assert_eq!(
                        tool.modes,
                        head.iter()
                            .find(|head| head.name == tool.name)
                            .unwrap()
                            .modes
                    );
                }
            }
        }
    }

    #[test]
    fn ledger_scope_owner_and_route_principal_have_identical_canonical_bytes() {
        for principal in [
            subc_protocol::Principal::Direct,
            subc_protocol::Principal::Reserved {
                module_id: "agent".into(),
            },
            subc_protocol::Principal::Unverified,
        ] {
            let stamp = subc_protocol::scope::ScopeStamp::new(
                principal.clone(),
                "scope",
                7,
                subc_protocol::scope::ScopeKind::Head,
            )
            .with_owner_authorized(true);
            assert_eq!(
                ledger_scope(&stamp).owner.as_bytes(),
                super::super::principal_id(&Some(principal))
                    .unwrap()
                    .as_bytes()
            );
        }
    }
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

    /// `role.describe` serves exactly the declaration pinned in the
    /// conformance fixture: `role.describe`, `tool.catalog` and `tool.call`
    /// plus `tool.withdraw`, `late_results` and `late_results.ack`, and the
    /// root-confinement capability used for worker admission.
    #[test]
    fn slice_b_declaration_matches_independent_fixture_without_normalizing_other_fields() {
        let fixtures: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/tool_provider_conformance.json"
        ))
        .unwrap();
        let mut actual = describe();
        assert_eq!(actual["implementation_version"], env!("CARGO_PKG_VERSION"));
        actual["implementation_version"] = json!("<built-package-version>");
        assert_eq!(actual, fixtures["slice_b"]["declaration"]);
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
                assert_eq!(tool.semantics, if tool.name == "bash" { 2 } else { 1 });
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

    /// Only a session with `revision_reads` on is offered `ref`, and only on
    /// read, outline and zoom; admission accepts a `ref` call from any session
    /// so the command can refuse it by name when the setting is off.
    #[test]
    fn only_revision_read_sessions_are_offered_ref() {
        for preset in CatalogPreset::ALL {
            for enabled in [false, true] {
                for tool in tools_for_session(preset, &[], true, false, enabled) {
                    let offered = tool.input_schema["properties"].get("ref").is_some();
                    let eligible = matches!(tool.name.as_str(), "read" | "outline" | "zoom");
                    assert_eq!(offered, enabled && eligible, "{preset:?} {}", tool.name);
                }
            }
        }
        // Fill the read schema's required path argument under whatever name
        // the catalog uses for it, then add `ref`.
        let read = tools_for_session(CatalogPreset::Head, &[], true, false, true)
            .into_iter()
            .find(|tool| tool.name == "read")
            .unwrap();
        let mut arguments = serde_json::Map::new();
        for name in read.input_schema["required"].as_array().unwrap() {
            arguments.insert(name.as_str().unwrap().into(), json!("src/a.rs"));
        }
        arguments.insert("ref".into(), json!("main"));
        let request = call("read", Value::Object(arguments));
        assert!(admit(&request, false, &[], true, "session", true).is_ok());
    }

    #[test]
    fn head_bash_calls_with_old_semantics_pins_are_refused() {
        let tool = tools_for_session(CatalogPreset::Head, &[], true, false, false)
            .into_iter()
            .find(|tool| tool.name == "bash")
            .unwrap();
        let mut request = call("bash", json!({"command":"echo"}));
        request.preset = Some("head".into());
        request.schema_pin = Some(
            SchemaPin::new("bash", &tool.schema_digest, 1)
                .encode()
                .unwrap(),
        );
        let refusal = admit(&request, false, &[], true, "session", true).unwrap_err();
        assert_eq!(refusal.code, "tool_semantics_changed");
        assert_eq!(
            refusal.detail.unwrap(),
            json!({"tool":"bash","expected":1,"current":2})
        );
        request.schema_pin = Some(
            SchemaPin::new("bash", &tool.schema_digest, 2)
                .encode()
                .unwrap(),
        );
        assert_eq!(
            admit(&request, false, &[], true, "session", true).unwrap(),
            CallerRole::Head
        );
        request.schema_pin = None;
        assert_eq!(
            admit(&request, false, &[], true, "session", true).unwrap(),
            CallerRole::Head
        );
    }

    #[test]
    fn worker_shell_calls_with_old_semantics_pins_are_refused() {
        for name in ["bash", "bash_watch"] {
            let tool = tools_for_session(CatalogPreset::Worker, &[], true, false, false)
                .into_iter()
                .find(|tool| tool.name == name)
                .unwrap();
            let arguments = if name == "bash" {
                json!({"command":"echo"})
            } else {
                json!({"taskId":"bash-0123456789abcdef"})
            };
            let mut request = call(name, arguments);
            request.preset = Some("worker".into());
            request.schema_pin = Some(
                SchemaPin::new(name, &tool.schema_digest, 1)
                    .encode()
                    .unwrap(),
            );
            let refusal = admit(&request, false, &[], true, "session", true).unwrap_err();
            assert_eq!(refusal.code, "tool_semantics_changed");
            assert_eq!(
                refusal.detail.unwrap(),
                json!({"tool":name,"expected":1,"current":2})
            );
            request.schema_pin = Some(
                SchemaPin::new(name, &tool.schema_digest, 2)
                    .encode()
                    .unwrap(),
            );
            assert_eq!(
                admit(&request, false, &[], true, "session", true).unwrap(),
                CallerRole::Worker
            );
            request.schema_pin = None;
            assert_eq!(
                admit(&request, false, &[], true, "session", true).unwrap(),
                CallerRole::Worker
            );
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
                fixture["reply"] = catalog_for_session(
                    fixture["request"].clone(),
                    &disabled,
                    fixture["powershell_available"].as_bool().unwrap(),
                    remote_runs(fixture),
                    crate::config::DEFAULT_BASH_WORKER_WAIT_MAX_MS,
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

    /// Whether a golden is for a session that may run commands remotely
    /// (absent means it may not).
    fn remote_runs(fixture: &Value) -> bool {
        fixture["remote_runs"].as_bool().unwrap_or(false)
    }

    fn check_catalog_golden(fixture: &Value) {
        let disabled: Vec<String> =
            serde_json::from_value(fixture["disabled_tools"].clone()).unwrap();
        let available = fixture["powershell_available"].as_bool().unwrap();
        let runon = remote_runs(fixture);
        let catalog = |request: Value, disabled: &[String], available: bool| {
            catalog_for_session(
                request,
                disabled,
                available,
                runon,
                crate::config::DEFAULT_BASH_WORKER_WAIT_MAX_MS,
            )
        };
        let request = fixture["request"].clone();
        let actual = catalog(request.clone(), &disabled, available).unwrap();
        // `runon` is in bash's schema exactly when the session may run remotely.
        let bash_has_runon = actual["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| tool["name"] == "bash")
            .any(|tool| tool["input_schema"]["properties"].get("runon").is_some());
        assert_eq!(
            bash_has_runon,
            runon
                && actual["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["name"] == "bash"),
            "{}",
            fixture["name"]
        );
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

    /// AFT offers no default system text, so a system-text item without a
    /// preset is refused, byte for byte as it was while the contract's decoder
    /// still required the preset.
    #[test]
    fn system_text_without_a_preset_is_refused_as_before() {
        for body in [
            json!({"system_text": {"params": {}}}),
            json!({"preset": "worker", "system_text": {"params": {"worker": true}}}),
            // The old decoder refused the missing preset before the unknown
            // catalog preset was ever looked at.
            json!({"preset": "bogus", "system_text": {"params": {}}}),
        ] {
            let error = catalog(body, &[], true).unwrap_err();
            assert_eq!(
                serde_json::to_value(&error).unwrap(),
                serde_json::to_value(errors::invalid_request(
                    "arguments",
                    "missing field `preset`"
                ))
                .unwrap()
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
    fn presetless_catalog_lists_exactly_the_served_presets() {
        let answer = catalog(json!({}), &[], false).unwrap();
        assert_eq!(answer["presets"], json!(["head", "worker", "reader"]));
        println!("PRESETLESS_CATALOG_JSON={answer}");
    }

    #[test]
    fn preset_specific_catalog_omits_presets() {
        for preset in CatalogPreset::ALL {
            let answer = catalog(json!({"preset": preset.name()}), &[], false).unwrap();
            assert!(answer.get("presets").is_none(), "{}", preset.name());
        }
    }

    #[test]
    fn head_preset_matches_presetless_content_except_discovery_and_fingerprint() {
        // Both requests serve head's tools and text. Only the preset-less
        // answer lists presets, so its content fingerprint is different.
        let without_discovery = |mut answer: Value| {
            for field in ["presets", "generation", "catalog_digest"] {
                answer.as_object_mut().unwrap().remove(field);
            }
            answer
        };
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
                assert_ne!(named["catalog_digest"], unnamed["catalog_digest"]);
                assert_eq!(
                    without_discovery(named),
                    without_discovery(unnamed),
                    "pwsh={available} worker={worker} text={text}"
                );
            }
            let bare = catalog(json!({}), &[], available).unwrap();
            assert_eq!(
                without_discovery(catalog(json!({"preset": "head"}), &[], available).unwrap()),
                without_discovery(bare.clone())
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

    /// Every `aft_<tool>` reference left in a served description, with whether
    /// the reply serves a tool under exactly that spelling.
    fn unresolved_references(reply: &Value) -> Vec<String> {
        let served = names(reply);
        let mut unresolved = Vec::new();
        for tool in reply["tools"].as_array().unwrap() {
            let text = tool["description"].as_str().unwrap_or_default();
            for reference in tool_references(text, &[]) {
                let spelled = &text[reference.start..reference.end];
                if !served.iter().any(|name| name == spelled) {
                    unresolved.push(format!("{}: {spelled}", tool["name"]));
                }
            }
        }
        unresolved
    }

    #[test]
    fn descriptions_name_only_tools_their_catalog_serves() {
        let disabled_sets: [&[&str]; 4] = [
            &[],
            &["aft_callgraph", "aft_inspect"],
            &["aft_zoom", "aft_search", "aft_safety"],
            &["aft_outline"],
        ];
        for preset in CatalogPreset::ALL {
            for disabled in disabled_sets {
                let disabled: Vec<String> = disabled.iter().map(|name| name.to_string()).collect();
                let reply = catalog(json!({"preset": preset.name()}), &disabled, true).unwrap();
                assert_eq!(
                    unresolved_references(&reply),
                    Vec::<String>::new(),
                    "{} disabled={disabled:?}",
                    preset.name()
                );
            }
        }
        // A reader catalog may serve outline, search and zoom without
        // callgraph; outline's description must not recommend callgraph then.
        let reply = catalog(
            json!({"preset": "reader"}),
            &["aft_callgraph".into(), "aft_inspect".into()],
            true,
        )
        .unwrap();
        let outline = description(&reply, "outline");
        assert!(!outline.contains("use callgraph"), "{outline}");
        assert!(
            outline.contains("prefer search + zoom on named symbols; use outline on a whole directory only for high-level structure mapping. zoom with `callgraph:true` gives one-level forward calls-out.\n"),
            "{outline}"
        );
        // Every tool served: only the spelling changes.
        let reply = catalog(json!({"preset": "reader"}), &[], true).unwrap();
        assert!(description(&reply, "outline").contains(
            "zoom with `callgraph:true` gives one-level forward calls-out; use callgraph only for reverse callers or multi-level traces."
        ));
    }

    /// Descriptions also name tools by their bare name, in two reference
    /// forms: "the `edit` tool" / "the grep tool", and "use `read`". A
    /// session that disabled `edit` but kept `write` must not be told to use
    /// the `edit` tool; a call to it would be refused.
    #[test]
    fn bare_tool_references_follow_the_served_set() {
        const WRITE_ADVICE: &str =
            "Use it to create files or replace whole contents; for partial edits, use the `edit` tool.";
        let reply = catalog(json!({}), &["edit".into()], true).unwrap();
        assert!(!names(&reply).contains(&"edit".to_string()));
        let write = description(&reply, "write");
        assert!(!write.contains("`edit` tool"), "{write}");
        assert!(
            write.contains("Use it to create files or replace whole contents. Write very large"),
            "{write}"
        );
        let reply = catalog(json!({}), &["read".into()], true).unwrap();
        let grep = description(&reply, "grep");
        assert!(!grep.contains("`read`"), "{grep}");
        assert!(
            grep.contains("Returns matching lines with file paths and line numbers. Always"),
            "{grep}"
        );
        // Control: with `edit` served, the advice stays word for word.
        let reply = catalog(json!({}), &[], true).unwrap();
        assert!(description(&reply, "write").contains(WRITE_ADVICE));
        assert!(description(&reply, "grep").contains("use `read` for that"));
        // A backticked tool name that is not in a reference form is an
        // argument, not a tool: zoom's `callgraph` stays with callgraph off.
        let reply = catalog(json!({}), &["aft_callgraph".into()], true).unwrap();
        assert!(description(&reply, "zoom")
            .contains("Top-level `contextLines` and `callgraph` apply to every entry"));
    }

    #[test]
    fn absent_tool_references_drop_the_smallest_readable_unit() {
        let served = ["zoom".to_string(), "outline".to_string()];
        let cases = [
            // A parenthetical goes with the space before it.
            (
                "Existing files are backed up (undo via aft_safety). Then more.",
                "Existing files are backed up. Then more.",
            ),
            // A `;` clause goes; a sentence that starts later is capitalized.
            (
                "prefer aft_search here; use aft_outline there.",
                "Use outline there.",
            ),
            (
                "aft_zoom gives calls-out; use aft_callgraph for callers.",
                "zoom gives calls-out.",
            ),
            // A sentence with no clause left goes, and so does a line left
            // empty, without leaving a run of blank lines.
            (
                "First.\n\nCall `aft_inspect` afterward.\n\nLast.",
                "First.\n\nLast.",
            ),
            (
                "- Keep\n- Backs up (via aft_safety undo)\n",
                "- Keep\n- Backs up\n",
            ),
            ("- Keep\n- Use aft_inspect\n", "- Keep\n"),
            // `aft_` before a name AFT does not define is plain text.
            ("See aft_unknown_tool.", "See aft_unknown_tool."),
            // The article follows the served name.
            (
                "Consider an aft_zoom pass, then an aft_outline.",
                "Consider a zoom pass, then an outline.",
            ),
            // Bare names count only in their reference forms.
            (
                "Rewrite it; for edits, use the `edit` tool. Pass `edit` or `callgraph` as text.",
                "Rewrite it. Pass `edit` or `callgraph` as text.",
            ),
            ("Lines only (use `read` for that).", "Lines only."),
            ("STOP — use the grep tool instead. Next.", "Next."),
            // An absent tool before a dash drops only the lead.
            (
                "To overwrite, use the `write` tool — `zoom` needs a mode. Next.",
                "`zoom` needs a mode. Next.",
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(resolve_tool_references(text, &served), expected, "{text}");
        }
    }

    /// The catalog advertises `path` for read, write and edit, but a session
    /// that started on the older catalog still sends `filePath`, possibly with
    /// that catalog's schema pin.
    #[test]
    fn old_catalog_file_path_calls_are_admitted_and_both_spellings_refused() {
        // The schema digests of the catalog that advertised `filePath`.
        const OLD_DIGESTS: [(&str, &str); 3] = [
            (
                "read",
                "ebd897cd5eb17d7ab3eaa037fafc9798cee370be1ea1309275d36cf99cca3b98",
            ),
            (
                "write",
                "183d52944d3d22b68816e38bd6bd165e8006b301b66765003240cc51f59bb1df",
            ),
            (
                "edit",
                "30cea47021d773a4abaa0409b9f1ec146a7dcac9673120fa8b372017b7575f71",
            ),
        ];
        let arguments = |name: &str, key: &str| match name {
            "read" => json!({key: "src/main.rs", "limit": 20}),
            "write" => json!({key: "src/main.rs", "content": "x"}),
            _ => json!({key: "src/main.rs", "edits": [{"oldString": "a", "newString": "b"}]}),
        };
        let reply = catalog(json!({}), &[], true).unwrap();
        for (name, old_digest) in OLD_DIGESTS {
            let entry = reply["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["name"] == name)
                .unwrap();
            assert!(entry["input_schema"]["properties"].get("path").is_some());
            assert!(entry["input_schema"]["properties"]
                .get("filePath")
                .is_none());
            assert_ne!(entry["schema_digest"], old_digest, "{name}");
            assert!(
                legacy_file_path_digests(name).contains(&old_digest.to_string()),
                "{name}: {:?}",
                legacy_file_path_digests(name)
            );
            for key in ["path", "filePath"] {
                admit_plain(
                    &call(name, arguments(name, key)),
                    &[],
                    true,
                    "session",
                    true,
                )
                .unwrap_or_else(|error| panic!("{name} {key}: {error:?}"));
            }
            let mut pinned = call(name, arguments(name, "filePath"));
            pinned.schema_pin = Some(SchemaPin::new(name, old_digest, 1).encode().unwrap());
            admit_plain(&pinned, &[], true, "session", true)
                .unwrap_or_else(|error| panic!("{name} old pin: {error:?}"));
            let mut both = arguments(name, "path");
            both["filePath"] = json!("src/main.rs");
            let error = admit_plain(&call(name, both), &[], true, "session", true).unwrap_err();
            assert_eq!(error.code, errors::INVALID_REQUEST, "{name}");
            assert_eq!(
                error.detail.unwrap(),
                json!({"field": "filePath", "conflicts_with": "path"})
            );
        }
        // Other tools take no `filePath`.
        let error = admit_plain(
            &call("grep", json!({"pattern": "x", "filePath": "src"})),
            &[],
            true,
            "session",
            true,
        )
        .unwrap_err();
        assert_eq!(error.detail.unwrap()["field"], "filePath");
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
                orchestrate::blocking_wait_cap_ms(true, 1_800_000),
                Some(1_800_000),
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
            call_mode: None,
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
            (schema.schema_digest.as_str(), 2, None),
            (
                "0000000000000000000000000000000000000000000000000000000000000000",
                2,
                Some("tool_schema_changed"),
            ),
            (
                schema.schema_digest.as_str(),
                1,
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
pub(in crate::subc) mod route_tests {
    mod carrier_samples;
    #[path = "../../rich_ui/admission/route_tests.rs"]
    mod operation_admission;

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
            tool_result_body: false,
            disabled_tools: Arc::new(disabled),
            scope,
            run_scope: std::sync::OnceLock::new(),
            call_mode: subc_protocol::call_mode::CallMode::Classic,
            made_tool_call: AtomicBool::new(false),
            run_owner: None,
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
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            41,
            1,
            7,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        exchange_frame_with_actor(frame, identity, ctx, metrics, dispatch).await
    }

    async fn exchange_frame_with_actor(
        frame: Frame,
        identity: RouteIdentity,
        ctx: Arc<AppContext>,
        metrics: &Arc<DispatchPathMetrics>,
        dispatch: DispatchFn,
    ) -> WriterFrame {
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(identity.root.clone(), ctx));
        let routes = HashMap::from([(route_key(41, 1), identity)]);
        // As on the frame loop, a scoped worker's saved confinement is read
        // off the loop first when memory cannot answer for it.
        let decoded = DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame,
            phase_trace: PhaseTrace::new(Instant::now()),
        };
        let mut waits = super::super::persistence::DatabaseWaits::default();
        let decoded = match waits.read_confinement(decoded, &routes, &executor) {
            Ok(()) => waits.next().await,
            Err(decoded) => decoded,
        };
        exchange_decoded_with_actor(decoded, &routes, &executor, metrics, dispatch).await
    }

    pub(in crate::subc) async fn exchange_decoded_with_actor(
        decoded: DecodedFrame,
        routes: &HashMap<RouteChannel, RouteIdentity>,
        executor: &Arc<Executor>,
        metrics: &Arc<DispatchPathMetrics>,
        dispatch: DispatchFn,
    ) -> WriterFrame {
        exchange_decoded_with_capabilities(
            decoded,
            routes,
            executor,
            metrics,
            dispatch,
            crate::subc::rich_ui::session::ProofCapabilities::from_manifest(
                &manifest::build_manifest(),
            ),
        )
        .await
    }

    pub(in crate::subc) async fn exchange_decoded_with_capabilities(
        decoded: DecodedFrame,
        routes: &HashMap<RouteChannel, RouteIdentity>,
        executor: &Arc<Executor>,
        metrics: &Arc<DispatchPathMetrics>,
        dispatch: DispatchFn,
        proofs: crate::subc::rich_ui::session::ProofCapabilities,
    ) -> WriterFrame {
        let (writer, mut replies) = mpsc::channel(8);
        let (bash_tx, _bash_rx) = mpsc::channel(8);
        let (touch_tx, _touch_rx) = mpsc::channel(8);
        let (deferred_tx, mut deferred_rx) = mpsc::unbounded_channel();
        let deferred_tx = super::super::DeferredResponseSender {
            entries: deferred_tx,
            wake: crate::response_finalize::DeferredResponseWake::default(),
        };
        let frame = decoded.frame;
        handle_tool_call_with_capabilities(
            &writer,
            &frame,
            None,
            decoded.confinement,
            decoded.phase_trace,
            routes,
            &HashMap::new(),
            &crate::subc::ReclaimedRoutes::default(),
            &mut HashMap::new(),
            executor,
            &Arc::default(),
            &Arc::new(PendingDeferredSetups::default()),
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
            None,
            proofs,
        )
        .await
        .unwrap();
        let mut reply = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::select! {
                reply = replies.recv() => reply.unwrap(),
                entry = deferred_rx.recv() => {
                    let mut entry = entry.expect("deferred tool setup");
                    let ctx = executor.actor_context(&entry.root).unwrap();
                    let response = loop {
                        if let Some(response) = (entry.pending.poll)(&ctx) {
                            break response;
                        }
                        deferred_tx.wake.notified().await;
                    };
                    deliver_resolved_subc_response(
                        &writer, ResolvedSubcResponse { entry, response }, routes,
                        &mut HashMap::new(), executor, &Arc::default(),
                        &Arc::new(Notify::new()), metrics, 1024 * 1024,
                    ).await.unwrap();
                    replies.recv().await.unwrap()
                }
            }
        })
        .await
        .unwrap();
        reply.project_tool_result().unwrap();
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

    fn occipital_identity(root: &ProjectRootId, role: RouteRole) -> RouteIdentity {
        let mut identity = test_support::route_identity(root, "session-s");
        let data = Arc::get_mut(&mut identity.0).unwrap();
        data.role = role;
        data.trust = BindTrust::Untrusted;
        data.scope = Some(stamp("worker"));
        data.spawn_principal = AuthenticatedPrincipal::RouteBind {
            trust: crate::sandbox_spawn::PrincipalTrust::Untrusted,
            route_channel: 41,
            route_epoch: 1,
            project_root: root.as_path().into(),
            harness: data.harness.clone(),
            session_id: data.session.clone(),
            principal_id: Some("reserved:occipital".into()),
        };
        identity
    }

    #[tokio::test]
    async fn occipital_refuses_unlisted_tools_before_dispatch() {
        let _guard = EXCHANGE_LOCK.lock().await;
        ACTIONS.store(0, Ordering::SeqCst);
        let (_dir, root) = test_support::test_root("occipital-denied");
        let mut names: Vec<_> = METADATA
            .iter()
            .map(|(name, _, _)| *name)
            .filter(|name| {
                !matches!(
                    *name,
                    "read"
                        | "grep"
                        | "glob"
                        | "bash_status"
                        | "outline"
                        | "zoom"
                        | "search"
                        | "callgraph"
                )
            })
            .collect();
        names.extend([
            "aft_inspect",
            "aft_delete",
            "aft_move",
            "aft_import",
            "aft_safety",
            "future_tool",
            "configure",
            "grants.revoke",
            "call.withdraw",
        ]);
        for role in [RouteRole::ToolProviderV1, RouteRole::Legacy] {
            for name in &names {
                let identity = occipital_identity(&root, role);
                let reply = exchange_with_actor(
                    json!({"name": name, "preset": "worker", "arguments": {}}),
                    identity,
                    test_support::test_ctx(),
                    &Arc::new(DispatchPathMetrics::new()),
                    |request, _| {
                        ACTIONS.fetch_add(1, Ordering::SeqCst);
                        Response::success(request.id, json!({}))
                    },
                )
                .await;
                let body: Value = serde_json::from_slice(&reply.body).unwrap();
                assert_eq!(reply.header.ty, FrameType::Error, "{name}: {body}");
                assert_eq!(body["code"], "carrier_read_only", "{name}: {body}");
                assert_eq!(
                    body["message"],
                    format!(
                        "This route is read-only: {name} is not available to the rich-UI provider."
                    )
                );
                assert_eq!(ACTIONS.load(Ordering::SeqCst), 0, "{name} dispatched");
            }
        }
    }

    #[tokio::test]
    async fn occipital_allowed_reads_require_versioned_operations_on_v1() {
        let _guard = EXCHANGE_LOCK.lock().await;
        let (_dir, root) = test_support::test_root("occipital-allowed");
        for role in [RouteRole::ToolProviderV1, RouteRole::Legacy] {
            for (name, arguments, internal) in [
                ("read", json!({"path":"main.rs"}), "read"),
                ("aft_outline", json!({"target":"main.rs"}), "outline"),
                (
                    "aft_zoom",
                    json!({"batch":[{"target":"main.rs","symbols":"main"}]}),
                    "zoom",
                ),
                ("aft_search", json!({"query":"main"}), "semantic_search"),
                ("grep", json!({"pattern":"main"}), "grep"),
                ("glob", json!({"pattern":"*.rs"}), "glob"),
                (
                    "aft_callgraph",
                    json!({"path":"main.rs","symbol":"main","op":"callers"}),
                    "callers",
                ),
            ] {
                *LAST_DISPATCH.lock().unwrap() = None;
                let identity = occipital_identity(&root, role);
                let reply = exchange_with_actor(
                    json!({"name":name,"preset":"worker","arguments":arguments}),
                    identity,
                    test_support::test_ctx(),
                    &Arc::new(DispatchPathMetrics::new()),
                    |request, _| {
                        assert!(matches!(
                            crate::access_policy::current_ask_route(),
                            crate::access_policy::AskRoute::Deny
                        ));
                        *LAST_DISPATCH.lock().unwrap() = Some(json!({
                            "command":request.command, "session":request.session(),
                        }));
                        Response::success(request.id, json!({"marker":"session-read"}))
                    },
                )
                .await;
                let body: Value = serde_json::from_slice(&reply.body).unwrap();
                assert_eq!(reply.header.ty, FrameType::Response, "{name}: {body}");
                if role == RouteRole::ToolProviderV1 {
                    assert_eq!(
                        body,
                        super::super::rich_ui::carrier::error_body("rich_ui_op_malformed")
                    );
                    assert_eq!(*LAST_DISPATCH.lock().unwrap(), None);
                    continue;
                }
                assert_eq!(body["isError"], false, "{name}: {body}");
                assert_eq!(
                    *LAST_DISPATCH.lock().unwrap(),
                    Some(json!({"command":internal,"session":"session-s"})),
                    "{name}"
                );
            }
        }
    }

    #[tokio::test]
    async fn occipital_refuses_future_tools_by_default() {
        let (_dir, root) = test_support::test_root("occipital-future-tool");
        for role in [RouteRole::ToolProviderV1, RouteRole::Legacy] {
            let reply = exchange_with_actor(
                json!({"name":"future_tool","preset":"worker","arguments":{}}),
                occipital_identity(&root, role),
                test_support::test_ctx(),
                &Arc::new(DispatchPathMetrics::new()),
                |_, _| panic!("unknown tools must not dispatch"),
            )
            .await;
            let body: Value = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(reply.header.ty, FrameType::Error, "{body}");
            assert_eq!(body["code"], "carrier_read_only", "{body}");
        }
    }

    #[tokio::test]
    async fn occipital_tool_refusal_precedes_target_refusal_for_both_result_bodies() {
        let (_dir, root) = test_support::test_root("occipital-refusal-order");
        for tool_result_body in [false, true] {
            for name in ["write", "future_tool"] {
                for target in ["https://example.com/doc", "issue://123", "pr://123"] {
                    let mut identity = occipital_identity(&root, RouteRole::ToolProviderV1);
                    Arc::get_mut(&mut identity.0).unwrap().tool_result_body = tool_result_body;
                    let reply = exchange_with_actor(
                        json!({"name":name,"preset":"worker","arguments":{"path":target,"content":"never written"}}),
                        identity, test_support::test_ctx(), &Arc::new(DispatchPathMetrics::new()),
                        |_, _| panic!("forbidden tool must not dispatch"),
                    ).await;
                    let body: Value = serde_json::from_slice(&reply.body).unwrap();
                    assert_eq!(reply.header.ty, FrameType::Error, "{body}");
                    assert_eq!(body["code"], "carrier_read_only", "{body}");
                    assert_eq!(body["message"], format!("This route is read-only: {name} is not available to the rich-UI provider."));
                }
            }
        }
    }

    #[tokio::test]
    async fn occipital_unpublished_read_uses_closed_carrier_body() {
        let _manifest = super::super::rich_ui::chunks::set_test_manifest_for_test(None);
        let (_dir, root) = test_support::test_root("occipital-tool-result");
        let mut identity = occipital_identity(&root, RouteRole::ToolProviderV1);
        Arc::get_mut(&mut identity.0).unwrap().tool_result_body = true;
        let reply = exchange_with_actor(
            json!({"name":"read","preset":"worker","arguments":{"path":"local.txt","rich_ui_op":"read@1"}}),
            identity,
            test_support::test_ctx(),
            &Arc::new(DispatchPathMetrics::new()),
            |_, _| panic!("unpublished operations must not dispatch"),
        )
        .await;
        assert_eq!(reply.header.ty, FrameType::Response);
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(
            body,
            super::super::rich_ui::carrier::error_body("operation_version_unsupported")
        );
        super::super::rich_ui::carrier::validate_body(None, &body).unwrap();
    }

    #[tokio::test]
    async fn occipital_refuses_management_operations_and_spoofed_identity() {
        let (_dir, root) = test_support::test_root("occipital-operations");
        for operation in [
            "grants.revoke",
            "grants.list",
            "call.withdraw",
            "call.pull",
            "configure",
            "health.digest",
            "bg_events",
            "future.operation",
        ] {
            let reply = exchange_with_actor(
                json!({"op":operation,"principal":"reserved:broca","params":{}}),
                occipital_identity(&root, RouteRole::ToolProviderV1),
                test_support::test_ctx(),
                &Arc::new(DispatchPathMetrics::new()),
                |_, _| panic!("refused operation must not dispatch"),
            )
            .await;
            let body: Value = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(reply.header.ty, FrameType::Error, "{operation}: {body}");
            assert_eq!(body["code"], "carrier_read_only", "{operation}: {body}");
        }
    }

    async fn occipital_network_target_case(name: &str, target: &str, kind: &str) {
        let _guard = EXCHANGE_LOCK.lock().await;
        ACTIONS.store(0, Ordering::SeqCst);
        let (_dir, root) = test_support::test_root("occipital-network-target");
        let arguments = match name {
            "read" => json!({"path": target}),
            "aft_outline" => json!({"target": target}),
            "aft_zoom" => json!({"batch":[{"target":target,"symbols":"Overview"}]}),
            _ => panic!("unexpected target-bearing tool"),
        };
        for role in [RouteRole::ToolProviderV1, RouteRole::Legacy] {
            let reply = exchange_with_actor(
                json!({"name":name,"preset":"worker","arguments":arguments}),
                occipital_identity(&root, role),
                test_support::test_ctx(),
                &Arc::new(DispatchPathMetrics::new()),
                |request, _| {
                    ACTIONS.fetch_add(1, Ordering::SeqCst);
                    Response::success(request.id, json!({}))
                },
            )
            .await;
            let body: Value = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(reply.header.ty, FrameType::Error, "{name} {target}: {body}");
            assert_eq!(body["code"], "carrier_read_only", "{body}");
            assert_eq!(body["message"], format!("This route is read-only: {kind} targets are not available to the rich-UI provider."));
            assert_eq!(
                ACTIONS.load(Ordering::SeqCst),
                0,
                "network target dispatched"
            );
        }
    }

    macro_rules! network_target_test {
        ($test:ident, $tool:literal, $target:literal, $kind:literal) => {
            #[tokio::test]
            async fn $test() {
                occipital_network_target_case($tool, $target, $kind).await;
            }
        };
    }

    network_target_test!(
        occipital_denies_read_issue_target,
        "read",
        "issue://123",
        "issue://"
    );
    network_target_test!(occipital_denies_read_pr_target, "read", "pr://123", "pr://");
    network_target_test!(
        occipital_denies_outline_http_target,
        "aft_outline",
        "http://example.com/doc",
        "URL"
    );
    network_target_test!(
        occipital_denies_outline_https_target,
        "aft_outline",
        "https://example.com/doc",
        "URL"
    );
    network_target_test!(
        occipital_denies_outline_issue_target,
        "aft_outline",
        "issue://123",
        "issue://"
    );
    network_target_test!(
        occipital_denies_outline_pr_target,
        "aft_outline",
        "pr://123",
        "pr://"
    );
    network_target_test!(
        occipital_denies_zoom_http_target,
        "aft_zoom",
        "http://example.com/doc",
        "URL"
    );
    network_target_test!(
        occipital_denies_zoom_https_target,
        "aft_zoom",
        "https://example.com/doc",
        "URL"
    );
    network_target_test!(
        occipital_denies_zoom_issue_target,
        "aft_zoom",
        "issue://123",
        "issue://"
    );
    network_target_test!(
        occipital_denies_zoom_pr_target,
        "aft_zoom",
        "pr://123",
        "pr://"
    );

    #[tokio::test]
    async fn occipital_network_target_aliases_cannot_bypass_admission() {
        let (_dir, root) = test_support::test_root("occipital-target-aliases");
        for (name, arguments) in [
            ("read", json!({"path":"issue://123"})),
            ("outline", json!({"target":["local.rs", "pr://123"]})),
            (
                "zoom",
                json!({"path":"http://example.com/doc","symbol":"Overview"}),
            ),
            (
                "zoom",
                json!({"url":"https://example.com/doc","symbols":"Overview"}),
            ),
            ("zoom", json!({"path":"pr://123","symbol":"1"})),
            (
                "zoom",
                json!({"targets":[{"url":"https://example.com/doc","symbol":"Overview"}]}),
            ),
            (
                "zoom",
                json!({"targets":{"path":"issue://123","symbol":"1"}}),
            ),
            (
                "zoom",
                json!({"batch":"[{\"target\":\"https://example.com/doc\",\"symbols\":\"Overview\"}]"}),
            ),
            (
                "zoom",
                json!({"batch":[{"target":"local.rs","symbols":"main"},{"url":"pr://123","symbols":"1"}]}),
            ),
        ] {
            let reply = exchange_with_actor(
                json!({"name":name,"preset":"worker","arguments":arguments}),
                occipital_identity(&root, RouteRole::Legacy),
                test_support::test_ctx(),
                &Arc::new(DispatchPathMetrics::new()),
                |_, _| panic!("remote target alias must not dispatch"),
            )
            .await;
            let body: Value = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(reply.header.ty, FrameType::Error, "{name}: {body}");
            assert_eq!(body["code"], "carrier_read_only", "{name}: {body}");
        }
    }

    #[tokio::test]
    async fn occipital_target_restrictions_do_not_affect_normal_routes() {
        let _guard = EXCHANGE_LOCK.lock().await;
        ACTIONS.store(0, Ordering::SeqCst);
        let (_dir, root) = test_support::test_root("normal-network-target");
        for (name, targets) in [
            ("read", vec!["issue://123", "pr://123"]),
            (
                "outline",
                vec![
                    "http://example.com/doc",
                    "https://example.com/doc",
                    "issue://123",
                    "pr://123",
                ],
            ),
            (
                "zoom",
                vec![
                    "http://example.com/doc",
                    "https://example.com/doc",
                    "issue://123",
                    "pr://123",
                ],
            ),
        ] {
            for target in targets {
                let arguments = match name {
                    "read" => json!({"path": target}),
                    "outline" => json!({"target": target}),
                    _ => json!({"batch":[{"target":target,"symbols":"Overview"}]}),
                };
                let mut identity = test_support::route_identity(&root, "normal-session");
                Arc::get_mut(&mut identity.0).unwrap().role = RouteRole::ToolProviderV1;
                let reply = exchange_with_actor(
                    json!({"name":name,"preset":"worker","arguments":arguments}),
                    identity,
                    test_support::test_ctx(),
                    &Arc::new(DispatchPathMetrics::new()),
                    |request, _| {
                        ACTIONS.fetch_add(1, Ordering::SeqCst);
                        Response::success(request.id, json!({}))
                    },
                )
                .await;
                let body: Value = serde_json::from_slice(&reply.body).unwrap();
                assert_eq!(
                    reply.header.ty,
                    FrameType::Response,
                    "{name} {target}: {body}"
                );
                assert_eq!(body["isError"], false, "{body}");
            }
        }
        assert_eq!(ACTIONS.load(Ordering::SeqCst), 10);
    }

    #[tokio::test]
    async fn occipital_catalog_is_filtered_before_digest_and_system_text() {
        let (_dir, root) = test_support::test_root("occipital-catalog");
        let ctx = test_support::test_ctx();
        for preset in ["head", "worker"] {
            let identity = occipital_identity(&root, RouteRole::ToolProviderV1);
            let reply = exchange_with_actor(
                json!({"op":"tool.catalog","preset":preset,"system_text":{"preset":preset}}),
                identity.clone(),
                ctx.clone(),
                &Arc::new(DispatchPathMetrics::new()),
                |_, _| panic!("catalog must not dispatch"),
            )
            .await;
            let catalog: Value = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(reply.header.ty, FrameType::Response, "{catalog}");
            let names: std::collections::BTreeSet<_> = catalog["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect();
            let expected: std::collections::BTreeSet<_> = [
                "read",
                "aft_outline",
                "aft_zoom",
                "aft_search",
                "grep",
                "glob",
                "aft_callgraph",
                "bash_status",
                "bash_watch",
            ]
            .into_iter()
            .collect();
            assert_eq!(names, expected);
            let text_names: std::collections::BTreeSet<_> = catalog["system_text"]["tool_names"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap())
                .collect();
            assert_eq!(text_names, expected);
            let digest = remote_policy::catalog(
                json!({"preset":preset,"system_text":{"preset":preset},"digest_only":true}),
                &identity,
                &ctx,
            )
            .unwrap();
            assert_eq!(digest["catalog_digest"], catalog["catalog_digest"]);
            let mut disabled = occipital_identity(&root, RouteRole::ToolProviderV1);
            Arc::get_mut(&mut disabled.0).unwrap().disabled_tools = Arc::new(vec!["read".into()]);
            let limited =
                remote_policy::catalog(json!({"preset":preset}), &disabled, &ctx).unwrap();
            assert!(!limited["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "read"));
            assert_ne!(limited["catalog_digest"], catalog["catalog_digest"]);
            assert!(remote_policy::catalog(
                json!({"preset":preset,"params":{"mode":"full"}}),
                &identity,
                &ctx
            )
            .is_err());
        }
    }

    #[tokio::test]
    async fn occipital_caller_claim_does_not_restrict_other_principals() {
        let _guard = EXCHANGE_LOCK.lock().await;
        ACTIONS.store(0, Ordering::SeqCst);
        let (_dir, root) = test_support::test_root("not-occipital");
        let reply = exchange_with_actor(
            json!({"name":"write", "preset":"worker", "arguments":{"path":"x","content":"x"}, "principal":"reserved:occipital"}),
            test_support::route_identity(&root, "session-s"),
            test_support::test_ctx(), &Arc::new(DispatchPathMetrics::new()),
            |request, _| { ACTIONS.fetch_add(1, Ordering::SeqCst); Response::success(request.id, json!({})) },
        ).await;
        assert_eq!(reply.header.ty, FrameType::Response);
        assert_eq!(ACTIONS.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn occipital_task_observation_is_limited_to_bound_session() {
        use crate::bash_background::persistence::{
            create_task_layout, write_task_at, PersistedTask,
        };
        use crate::bash_background::BgTaskStatus;

        let (dir, root) = test_support::test_root("occipital-tasks");
        let ctx = test_support::test_ctx();
        let storage = dir.path().join("tasks");
        for (session, id, output) in [
            ("session-s", "bash-0123456789abcdef", "own-output-marker"),
            (
                "session-other",
                "bash-fedcba9876543210",
                "foreign-output-marker",
            ),
        ] {
            // Allocate the bundle through the locked production path so the
            // session directory is non-empty before the detached persisted-task
            // GC from the first replay can see it. A bare task_paths +
            // write_task leaves an empty session directory that the sweep can
            // remove between creation and the first file write.
            let task = create_task_layout(&storage, session, id).unwrap();
            let paths = &task.paths;
            let mut metadata = PersistedTask::starting(
                id.into(),
                session.into(),
                "echo output".into(),
                root.as_path().into(),
                Some(root.as_path().into()),
                None,
                false,
                false,
            );
            metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
            write_task_at(&task, &metadata).unwrap();
            std::fs::write(&paths.stdout, output).unwrap();
            std::fs::write(&paths.stderr, "").unwrap();
            std::fs::write(&paths.exit, "0\n").unwrap();
            ctx.bash_background()
                .replay_session(&storage, session)
                .unwrap();
        }
        for role in [RouteRole::ToolProviderV1, RouteRole::Legacy] {
            for name in ["bash_status", "bash_watch"] {
                for (id, own) in [
                    ("bash-0123456789abcdef", true),
                    ("bash-fedcba9876543210", false),
                ] {
                    let identity = occipital_identity(&root, role);
                    let reply = exchange_with_actor(
                        json!({"name":name,"preset":"worker","arguments":{"taskId":id}}),
                        identity,
                        ctx.clone(),
                        &Arc::new(DispatchPathMetrics::new()),
                        |request, ctx| crate::commands::bash_status::handle(&request, ctx),
                    )
                    .await;
                    let body: Value = serde_json::from_slice(&reply.body).unwrap();
                    assert_eq!(reply.header.ty, FrameType::Response, "{name}: {body}");
                    if role == RouteRole::ToolProviderV1 {
                        assert_eq!(
                            body,
                            super::super::rich_ui::carrier::error_body("rich_ui_op_malformed")
                        );
                        continue;
                    }
                    let rendered = body.to_string();
                    if own {
                        assert_eq!(body["isError"], false, "{name}: {body}");
                        assert!(rendered.contains("own-output-marker"), "{name}: {body}");
                    } else {
                        assert_eq!(body["isError"], true, "{name}: {body}");
                        assert!(
                            !rendered.contains("foreign-output-marker"),
                            "{name}: {body}"
                        );
                    }
                }
            }
        }
    }

    fn task_poll_identity(root: &ProjectRootId, session: &str, carrier: bool) -> RouteIdentity {
        let mut identity = if carrier {
            occipital_identity(root, RouteRole::ToolProviderV1)
        } else {
            test_support::route_identity(root, session)
        };
        let data = Arc::get_mut(&mut identity.0).unwrap();
        // Exercise the observation handlers through the legacy decoder while
        // v1 carrier operation versions remain unpublished.
        data.role = if carrier {
            RouteRole::Legacy
        } else {
            RouteRole::ToolProviderV1
        };
        data.scope = Some(stamp("worker"));
        data.session = session.into();
        if let AuthenticatedPrincipal::RouteBind { session_id, .. } = &mut data.spawn_principal {
            *session_id = session.into();
        }
        identity
    }

    async fn task_poll_deadline_case(name: &str) {
        use crate::bash_background::persistence::read_task;
        for carrier in [true, false] {
            let (dir, ctx, task) =
                crate::bash_background::registry::default_kill_tests::wait_fixture(None);
            ctx.update_config(|config| config.bash.worker_wait_max_ms = 3_600_000);
            let root = ProjectRootId::from_path(dir.path()).unwrap();
            let before = std::fs::read(&task.paths.json).unwrap();
            let before_limit = read_task(&task.paths.json).unwrap().timeout_ms.unwrap();
            let ctx = Arc::new(ctx);
            let mut arguments = json!({"taskId":task.task_id});
            if name == "bash_watch" {
                arguments["timeoutMs"] = json!(1);
            }
            let reply = exchange_with_actor(
                json!({"name":name,"preset":"worker","arguments":arguments}),
                task_poll_identity(&root, &task.session_id, carrier),
                ctx.clone(),
                &Arc::new(DispatchPathMetrics::new()),
                |request, ctx| crate::commands::bash_status::handle(&request, ctx),
            )
            .await;
            let body: Value = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(reply.header.ty, FrameType::Response, "{name}: {body}");
            assert_eq!(body["isError"], false, "{name}: {body}");
            let after = read_task(&task.paths.json).unwrap();
            let live_limit = ctx
                .bash_background()
                .hard_kill_deadline(&task.task_id, &task.session_id)
                .unwrap()
                .limit_ms;
            if carrier {
                assert_eq!(
                    after.timeout_ms,
                    Some(before_limit),
                    "carrier {name} changed the persisted deadline"
                );
                assert_eq!(
                    live_limit, before_limit,
                    "carrier {name} changed the live deadline"
                );
                assert_eq!(
                    std::fs::read(&task.paths.json).unwrap(),
                    before,
                    "carrier {name} rewrote task metadata"
                );
            } else {
                assert!(
                    after.timeout_ms.unwrap() > before_limit,
                    "worker {name} did not persist its renewal"
                );
                assert!(live_limit > before_limit, "worker {name} did not renew");
            }
        }
    }

    #[tokio::test]
    async fn occipital_status_does_not_renew_but_worker_status_does() {
        task_poll_deadline_case("bash_status").await;
    }

    #[tokio::test]
    async fn occipital_watch_does_not_renew_but_worker_watch_does() {
        task_poll_deadline_case("bash_watch").await;
    }

    #[tokio::test]
    async fn occipital_watch_does_not_acknowledge_worker_completion() {
        use crate::bash_background::{
            persistence::{read_task, write_task},
            BgTaskStatus,
        };
        for carrier in [true, false] {
            let (dir, ctx, task) =
                crate::bash_background::registry::default_kill_tests::wait_fixture(None);
            {
                let mut state = task.lock_state().unwrap();
                state
                    .metadata
                    .mark_terminal(BgTaskStatus::Completed, Some(0), None);
                state.metadata.completion_delivered = false;
                write_task(&task.paths.json, &state.metadata).unwrap();
            }
            let root = ProjectRootId::from_path(dir.path()).unwrap();
            let reply = exchange_with_actor(
                json!({"name":"bash_watch","preset":"worker","arguments":{"taskId":task.task_id,"timeoutMs":1}}),
                task_poll_identity(&root, &task.session_id, carrier), Arc::new(ctx),
                &Arc::new(DispatchPathMetrics::new()),
                |_, _| panic!("watch uses the deferred handler"),
            ).await;
            let body: Value = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(body["isError"], false, "{body}");
            assert_eq!(
                read_task(&task.paths.json).unwrap().completion_delivered,
                !carrier,
                "only the worker's watch may acknowledge its completion"
            );
        }
    }

    #[tokio::test]
    async fn occipital_cold_task_reads_do_not_register_or_rewrite_tasks() {
        for name in ["bash_status", "bash_watch"] {
            let (dir, original, task) =
                crate::bash_background::registry::default_kill_tests::wait_fixture(None);
            std::fs::write(&task.paths.stdout, "observed-output-marker").unwrap();
            std::fs::write(&task.paths.stderr, "").unwrap();
            let ctx = Arc::new(AppContext::new(
                Box::new(crate::parser::TreeSitterProvider::new()),
                original.config().as_ref().clone(),
            ));
            let root = ProjectRootId::from_path(dir.path()).unwrap();
            let before = std::fs::read(&task.paths.json).unwrap();
            let mut arguments = json!({"taskId":task.task_id});
            if name == "bash_watch" {
                arguments["pattern"] = json!("observed-output-marker");
                arguments["timeoutMs"] = json!(100);
            }
            let reply = exchange_with_actor(
                json!({"name":name,"preset":"worker","arguments":arguments}),
                task_poll_identity(&root, &task.session_id, true),
                ctx.clone(),
                &Arc::new(DispatchPathMetrics::new()),
                |request, ctx| crate::commands::bash_status::handle(&request, ctx),
            )
            .await;
            let body: Value = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(reply.header.ty, FrameType::Response, "{name}: {body}");
            assert_eq!(body["isError"], false, "{name}: {body}");
            if name == "bash_watch" {
                assert!(
                    body.to_string().contains("observed-output-marker"),
                    "{name}: {body}"
                );
            } else {
                assert!(body.to_string().contains("running"), "{name}: {body}");
            }
            assert!(
                ctx.bash_background().task_for_test(&task.task_id).is_none(),
                "carrier {name} registered the task and could re-arm its durable watches"
            );
            assert_eq!(
                std::fs::read(&task.paths.json).unwrap(),
                before,
                "carrier {name} rewrote the task bundle"
            );
        }
    }

    #[tokio::test]
    async fn carried_unconfined_non_shell_is_refused_after_ledger_wait_and_older_epoch_save() {
        carried_unconfined_after_ledger_wait("write", json!({"filePath":"action","content":"x"}))
            .await;
    }

    #[tokio::test]
    async fn carried_unconfined_bash_is_refused_after_ledger_wait_and_older_epoch_save() {
        carried_unconfined_after_ledger_wait("bash", json!({"command":"true"})).await;
    }

    async fn carried_unconfined_after_ledger_wait(name: &str, arguments: Value) {
        for memoized in [false, true] {
            let (dir, root) = test_support::test_root("carried-policy-ledger-wait");
            let ctx = test_support::test_ctx();
            ctx.set_db(Arc::new(std::sync::Mutex::new(
                crate::db::open(&dir.path().join("aft.db")).unwrap(),
            )));
            let mut older = test_support::route_identity(&root, "worker");
            let inner = Arc::get_mut(&mut older.0).unwrap();
            inner.role = RouteRole::ToolProviderV1;
            inner.scope = Some(stamp("worker"));
            inner.spawn_principal = AuthenticatedPrincipal::RouteBind {
                trust: crate::sandbox_spawn::PrincipalTrust::FirstParty,
                route_channel: 41,
                route_epoch: 1,
                project_root: root.as_path().into(),
                harness: inner.harness.clone(),
                session_id: inner.session.clone(),
                principal_id: Some("reserved:worker".into()),
            };
            remote_policy::catalog(json!({"preset":"worker"}), &older, &ctx).unwrap();
            let mut newer = test_support::route_identity(&root, "worker");
            let inner = Arc::get_mut(&mut newer.0).unwrap();
            inner.role = older.role;
            inner.scope = older.scope.clone();
            inner.scope.as_mut().unwrap().scope_epoch += 1;
            inner.spawn_principal = older.spawn_principal.clone();
            if memoized {
                assert_eq!(remote_policy::confined(&ctx, &newer), Ok(false));
            }
            let executor = Arc::new(Executor::new());
            assert!(executor.register_actor(root.clone(), ctx.clone()));
            let routes = HashMap::from([(route_key(41, 1), newer.clone())]);
            let decoded = DecodedFrame {
                parsed_body: Default::default(),
                confinement: None,
                launch_restore: None,
                codemode_policies: None,
                frame: Frame::build(
                    FrameType::Request,
                    control_flags(),
                    41,
                    1,
                    7,
                    serde_json::to_vec(&json!({"name":name,"arguments":arguments,
                        "preset":"worker","call_key":"carried-policy"}))
                    .unwrap(),
                )
                .unwrap(),
                phase_trace: PhaseTrace::new(Instant::now()),
            };
            let mut waits = persistence::DatabaseWaits::default();
            let decoded = match waits.read_confinement(decoded, &routes, &executor) {
                Ok(()) => waits.next().await,
                Err(decoded) => decoded,
            };
            assert_eq!(
                decoded.confinement.as_ref().map(|read| &read.read),
                Some(&Ok(false))
            );

            // Keep the keyed call in the ledger's mutating lane while an older
            // epoch saves a fence inherited by the later epoch with no row.
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let held = executor.submit_async(
                root,
                crate::executor::Lane::Mutating,
                "held-ledger-lane".into(),
                Box::new(move |_| {
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    Response::success("held-ledger-lane", json!({}))
                }),
            );
            started_rx.await.unwrap();
            let (edge, mut ready) = LedgerEdge::new();
            let (writer, _replies) = mpsc::channel(8);
            let metrics = Arc::new(DispatchPathMetrics::new());
            assert!(edge
                .defer(decoded, &routes, &executor, &writer, &metrics)
                .is_ok());
            assert!(ready.try_recv().is_err());
            remote_policy::catalog(
                json!({"preset":"worker","params":{"confine":"root"}}),
                &older,
                &ctx,
            )
            .unwrap();
            assert_eq!(remote_policy::confinement_from_memory(&ctx, &newer), None);
            release_tx.send(()).unwrap();
            held.await.unwrap();
            let decoded = tokio::time::timeout(Duration::from_secs(5), ready.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(!edge.finish_waiting(&decoded.frame));
            let mut admitted_name = name.to_owned();
            let admission = admit_confined_call_name(
                &executor,
                &newer,
                &mut admitted_name,
                decoded.confinement.as_ref(),
            );
            assert!(
                admission.is_err(),
                "{name} admitted a stale unconfined answer, memoized={memoized}"
            );
            let reply =
                exchange_decoded_with_actor(decoded, &routes, &executor, &metrics, |request, _| {
                    Response::success(request.id, json!({}))
                })
                .await;
            assert_eq!(
                reply.header.ty,
                FrameType::Error,
                "{name}, memoized={memoized}"
            );
            let error: subc_protocol::ErrorBody = serde_json::from_slice(&reply.body).unwrap();
            assert!(
                matches!(
                    error.code.as_str(),
                    "tool_not_offered_confined" | "confine_policy_unavailable"
                ),
                "{name}, memoized={memoized}: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn untrusted_shell_capability_refusal_precedes_unavailable_confinement() {
        for opening in [false, true] {
            for keyed in [false, true] {
                let (_dir, root) = test_support::test_root("untrusted-shell-policy-order");
                let ctx = test_support::test_ctx();
                ctx.mark_database_runtime_initializing_for_test();
                if !opening {
                    ctx.finish_database_runtime_error("unreadable database".into(), false);
                }
                let mut identity = test_support::route_identity(&root, "session");
                let inner = Arc::get_mut(&mut identity.0).unwrap();
                inner.role = RouteRole::ToolProviderV1;
                inner.trust = BindTrust::Untrusted;
                inner.consumer_elicitation_capable = false;
                inner.scope = Some(stamp("worker"));
                let mut body = json!({"name":"bash", "arguments":{"command":"true"}});
                if keyed {
                    body["call_key"] = json!("no-elicitation");
                }
                // With `opening`, the database never finishes opening, so a
                // refusal that waited for it would hang past any bound. The
                // generous bound only absorbs actor startup on a loaded
                // Windows runner, where one second was not enough.
                let reply = tokio::time::timeout(
                    Duration::from_secs(15),
                    exchange_with_actor(
                        body,
                        identity,
                        ctx,
                        &Arc::new(DispatchPathMetrics::new()),
                        |_, _| panic!("a refused shell must never dispatch"),
                    ),
                )
                .await
                .expect("a capability refusal must not wait for the database");
                assert_eq!(reply.header.ty, FrameType::Error);
                let error: Value = serde_json::from_slice(&reply.body).unwrap();
                assert_eq!(error["code"], "capability_not_admitted");
            }
        }
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
        ctx.update_config(|config| config.sandbox.profile.head = crate::config::SandboxMode::Off);
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
            tool_result_body: false,
            disabled_tools: Arc::default(),
            scope: None,
            run_scope: std::sync::OnceLock::new(),
            call_mode: subc_protocol::call_mode::CallMode::Classic,
            made_tool_call: AtomicBool::new(false),
            run_owner: None,
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
                    host_tool_names: None,
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
                let _ = heavy_release.recv();
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
            test_support::route_identity(&root, "catalog-session"),
            &executor,
            &Arc::default(),
            &Arc::new(DispatchPathMetrics::new()),
            "tool.catalog",
            json!({}),
            PhaseTrace::new(Instant::now()),
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(80), replies.recv())
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
        let trace = reply
            .tool_response_trace
            .expect("tool.catalog replies must carry response timing");
        let now = Instant::now();
        let completed = trace
            .finish(now, now, now, reply.frame.body.len(), None)
            .expect("tool.catalog trace must have executor and writer timings");
        assert!(completed.phases.total > Duration::from_millis(50));
        assert!(completed.phases.queue > Duration::from_millis(50));
        let _ = crate::logging::take_slow_tool_call_logs_for_test();
        crate::log_ctx::with_session(Some(completed.session.clone()), || {
            crate::logging::note_tool_call_trace(
                &completed.name,
                &completed.session,
                &completed.root,
                completed.channel,
                completed.corr,
                completed.phases,
                crate::logging::ToolCallCaller::Agent,
            );
        });
        let lines = crate::logging::take_slow_tool_call_logs_for_test();
        let slow_line = lines
            .iter()
            .find(|line| line.contains("slow tool_call name=tool.catalog"))
            .expect("delayed catalog response should produce a slow tool_call log");
        assert!(slow_line.contains("session=catalog-session"));
        assert!(slow_line.contains("queue="));
        assert!(slow_line.contains("exec="));
        assert!(slow_line.contains("egress="));
        assert!(ctx.database_runtime_pending("write"));
        assert_eq!(executor.heavy_permits(), 1);
        release_heavy.send(()).unwrap();
        assert!(heavy.await.unwrap().success);
    }

    /// Sends `body` through the ledger edge, the frame loop's first stop for a
    /// request on a bound route, over a route with a real `aft.db`. `None`
    /// means the edge left the request to the ordinary tool-call handler.
    async fn custody_exchange(
        body: Value,
        role: RouteRole,
        scope: Option<subc_protocol::scope::ScopeStamp>,
    ) -> Option<WriterFrame> {
        let (dir, root) = test_support::test_root("custody-ops");
        let ctx = test_support::test_ctx();
        ctx.set_db(Arc::new(std::sync::Mutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        )));
        let mut identity = test_support::route_identity(&root, "session");
        let data = Arc::get_mut(&mut identity.0).unwrap();
        data.role = role;
        data.scope = scope;
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root, ctx));
        let routes = HashMap::from([(route_key(41, 1), identity)]);
        let (edge, _ready) = LedgerEdge::new();
        let decoded = DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame: Frame::build(
                FrameType::Request,
                control_flags(),
                41,
                1,
                7,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap(),
            phase_trace: PhaseTrace::new(Instant::now()),
        };
        let (writer, mut replies) = mpsc::channel(8);
        edge.defer(
            decoded,
            &routes,
            &executor,
            &writer,
            &Arc::new(DispatchPathMetrics::new()),
        )
        .ok()?;
        let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.header.corr, 7);
        Some(reply)
    }

    #[tokio::test]
    async fn unscoped_v1_route_runs_plain_calls_and_serves_custody_ops() {
        // A route the daemon stamped with no scope may run plain calls; the
        // conformance suite runs its plain cases on such a route. The custody
        // ops are served on it too: a withdraw there looks up the record keyed
        // on the route's own principal (the principal that raised the call),
        // and late results go to the route's principal as custodian.
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
        let withdraw = custody_exchange(
            json!({"name":"tool.withdraw", "arguments":{"call_key":"key"}}),
            RouteRole::ToolProviderV1,
            None,
        )
        .await
        .expect("v1 routes serve tool.withdraw");
        assert_eq!(withdraw.header.ty, FrameType::Response);
        assert_eq!(
            serde_json::from_slice::<Value>(&withdraw.body).unwrap(),
            json!({"answer": "unknown_call"})
        );
        let page = custody_exchange(
            json!({"name":"late_results", "arguments":{"since":null}}),
            RouteRole::ToolProviderV1,
            None,
        )
        .await
        .expect("v1 routes serve late_results");
        assert_eq!(page.header.ty, FrameType::Response);
        let page: Value = serde_json::from_slice(&page.body).unwrap();
        assert_eq!(page["entries"], json!([]));
        assert_eq!(page["more"], false);
        assert_eq!(
            page["cursor"],
            json!({"provider_incarnation": crate::db::call_ledger::provider_incarnation(), "seq": 0})
        );
        let malformed = custody_exchange(
            json!({"name":"late_results.ack", "arguments":{"through":"cursor"}}),
            RouteRole::ToolProviderV1,
            None,
        )
        .await
        .expect("v1 routes serve late_results.ack");
        let error = error_body(&malformed);
        assert_eq!(error.code, "invalid_request");
        assert_eq!(error.detail.unwrap()["field"], "arguments");
        assert_eq!(ACTIONS.load(Ordering::SeqCst), 1);
    }

    /// Legacy routes keep refusing the custody ops by name: the ledger edge
    /// leaves them to the tool-call handler, which answers
    /// `unsupported_operation` without dispatching anything.
    #[tokio::test]
    async fn legacy_routes_refuse_custody_ops_by_name() {
        let _guard = EXCHANGE_LOCK.lock().await;
        ACTIONS.store(0, Ordering::SeqCst);
        for body in [
            json!({"name":"tool.withdraw", "arguments":{"call_key":"key"}}),
            json!({"name":"late_results", "arguments":{"since":null}}),
            json!({"name":"late_results.ack", "arguments":{"through":{"provider_incarnation":"x","seq":0}}}),
        ] {
            for scope in [None, Some(stamp("head"))] {
                assert!(
                    custody_exchange(body.clone(), RouteRole::Legacy, scope.clone())
                        .await
                        .is_none(),
                    "{body}"
                );
                let reply =
                    exchange_with_scope(body.clone(), RouteRole::Legacy, "session", vec![], scope)
                        .await;
                assert_eq!(error_body(&reply).code, "unsupported_operation", "{body}");
            }
        }
        assert_eq!(ACTIONS.load(Ordering::SeqCst), 0);
    }

    fn stamp(kind: &str) -> subc_protocol::scope::ScopeStamp {
        serde_json::from_value(json!({"owner": {"kind": "direct"}, "ref": "scope", "scope_epoch": 1, "kind": kind, "owner_authorized": true})).unwrap()
    }

    /// A scoped worker route on its own project and aft.db, with `pin.rs` in
    /// the project. The temporary directories must outlive the route.
    fn worker_route(
        role: RouteRole,
        call_mode: subc_protocol::call_mode::CallMode,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Arc<AppContext>,
        RouteIdentity,
    ) {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = project.path().canonicalize().unwrap();
        std::fs::write(root.join("pin.rs"), "pub fn pinned_symbol() {}\n").unwrap();
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.clone()),
                storage_dir: Some(storage.path().into()),
                restrict_to_project_root: false,
                ..Default::default()
            },
        ));
        ctx.set_db(Arc::new(std::sync::Mutex::new(
            crate::db::open(&storage.path().join("aft.db")).unwrap(),
        )));
        let identity = RouteIdentity(Arc::new(RouteIdentityData {
            root: ProjectRootId::from_path(&root).unwrap(),
            project_root: root.clone(),
            harness: "runner".into(),
            session: "designer".into(),
            role,
            trust: BindTrust::FirstParty,
            spawn_principal: AuthenticatedPrincipal::RouteBind {
                trust: PrincipalTrust::FirstParty,
                route_channel: 41,
                route_epoch: 1,
                project_root: root.clone(),
                harness: "runner".into(),
                session_id: "designer".into(),
                principal_id: Some("reserved:broca".into()),
            },
            consumer_elicitation_capable: false,
            tool_result_body: false,
            disabled_tools: Arc::new(vec![]),
            scope: Some(stamp("worker")),
            run_scope: std::sync::OnceLock::new(),
            call_mode,
            made_tool_call: AtomicBool::new(false),
            run_owner: None,
        }));
        (project, storage, ctx, identity)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn direct_edit_carries_worker_and_untrusted_tool_policy_without_jailing_shared_services()
    {
        use crate::config::SandboxMode;
        let _guard = EXCHANGE_LOCK.lock().await;
        fn command_mode(policy: &crate::sandbox_spawn::ToolLaunchPolicy) -> String {
            let command = policy
                .command(std::ffi::OsStr::new("formatter-probe"), &[])
                .unwrap();
            if command.get_args().next() == Some(std::ffi::OsStr::new("sandbox-launch")) {
                let profile: crate::sandbox_profile::SandboxProfile =
                    serde_json::from_str(command.get_args().nth(2).unwrap().to_str().unwrap())
                        .unwrap();
                profile.mode.to_string()
            } else {
                "off".into()
            }
        }
        fn observe(request: RawRequest, ctx: &AppContext) -> Response {
            let policy = crate::sandbox_spawn::ToolLaunchPolicy::for_request(ctx);
            let mode = command_mode(&policy);
            // LSP startup and other shared services see the original scopes,
            // not the formatter's per-call principal or worker jail.
            assert!(!crate::sandbox_spawn::worker_jail_active());
            assert_eq!(
                crate::sandbox_spawn::current_authenticated_principal(),
                AuthenticatedPrincipal::FirstParty
            );
            Response::success(request.id, json!({"tool_mode": mode}))
        }
        for (worker_mode, untrusted) in [
            (SandboxMode::Off, false),
            (SandboxMode::Relaxed, false),
            (SandboxMode::Strict, false),
            (SandboxMode::Relaxed, true),
        ] {
            let (_project, storage, ctx, mut identity) = worker_route(
                RouteRole::ToolProviderV1,
                subc_protocol::call_mode::CallMode::Classic,
            );
            let operator_binary = storage.path().join("operator-aft");
            std::fs::write(&operator_binary, "operator executable fixture").unwrap();
            let _launcher = crate::sandbox_spawn::install_sandbox_launcher_for_test(
                &identity.project_root,
                operator_binary.clone(),
            );
            ctx.update_config(|config| {
                config.gh_shim.binary_path = Some(operator_binary);
                config.sandbox.profile.head = if worker_mode == SandboxMode::Off {
                    SandboxMode::Strict
                } else {
                    SandboxMode::Off
                };
                config.sandbox.profile.worker = worker_mode;
            });
            let data = Arc::get_mut(&mut identity.0).unwrap();
            let mut scope = stamp("worker");
            scope.owner = Principal::Reserved {
                module_id: "prefrontal-core".into(),
            };
            data.scope = Some(scope);
            if untrusted {
                data.trust = BindTrust::Untrusted;
                if let AuthenticatedPrincipal::RouteBind { trust, .. } = &mut data.spawn_principal {
                    *trust = PrincipalTrust::Untrusted;
                }
            }
            assert!(super::super::access_grants::alfonso_worker(&identity));
            let expected = if untrusted {
                SandboxMode::Strict
            } else {
                worker_mode
            };
            let run = PlainToolRun {
                speech_seed: None,
                bare_name: "write".into(),
                arguments: json!({"path": "pin.rs", "content": "pub fn edited() {}\n"}),
                format_context: Default::default(),
                tool_call_context: ToolCallContext {
                    project_root: identity.project_root.clone(),
                    session_id: Some(identity.session.clone()),
                    request_id: "tool-policy-dispatch".into(),
                    diagnostics_on_edit: false,
                    preview: false,
                    edit_slot_survives: None,
                    report_registration_downgrade: false,
                    standard_edit_grammar: true,
                    disabled_tools: None,
                    worker_session: true,
                    host_tool_names: None,
                },
                dispatch: observe,
                bind_trust: identity.trust,
                identity,
                request_id: "tool-policy-dispatch".into(),
                ledger_key: None,
                io_budget: crate::bounded_io::Budget::after(Duration::from_secs(30)),
                approved_io_window: None,
                reply_deadline: Instant::now() + Duration::from_secs(30),
            };
            let (done, completion) = oneshot::channel();
            let job = run.job(
                crate::access_policy::AskRoute::Lowered,
                PhaseTrace::new(Instant::now()),
                done,
            );
            let reads = usize::from(
                expected == SandboxMode::Strict
                    || (expected == SandboxMode::Relaxed && !cfg!(target_os = "linux")),
            );
            crate::worker_jail_canary::with_test_preparation_home(storage.path(), reads, || {
                let response = job(&ctx);
                assert_eq!(
                    response.data["tool_mode"],
                    expected.to_string(),
                    "{}",
                    response.data
                );
            });
            completion.await.unwrap();
            // A later standalone/head request must not inherit the worker policy.
            let head = crate::sandbox_spawn::ToolLaunchPolicy::for_request(&ctx);
            assert_eq!(
                command_mode(&head),
                ctx.config().sandbox.profile.head.to_string()
            );
        }
    }

    /// Building a worker's formatter launch policy creates a private task
    /// directory and a full sandbox profile, which used to happen before
    /// dispatch on every mutating worker call. Calls that launch no formatter
    /// or checker must not build it at all; a call that launches tools builds
    /// it once and shares it across its commands; and a policy that cannot be
    /// built refuses with exactly the text it gave when it was built up front.
    #[cfg(unix)]
    #[tokio::test]
    async fn worker_edit_builds_tool_launch_policy_only_when_a_tool_launches() {
        use crate::sandbox_spawn::{SandboxTaskKind, ToolLaunchPolicy};
        let _guard = EXCHANGE_LOCK.lock().await;
        /// Content `launch` makes the call build a formatter command and,
        /// through a second policy handle, a checker command (building a
        /// command is what resolves the policy); any other content builds no
        /// tool command.
        fn observe(request: RawRequest, ctx: &AppContext) -> Response {
            let policy = ToolLaunchPolicy::for_request(ctx);
            if request.params["content"].as_str() != Some("launch") {
                return Response::success(request.id, json!({"launched": false}));
            }
            let formatter = policy
                .command(std::ffi::OsStr::new("formatter-probe"), &[])
                .map(|_| ());
            let nested = ToolLaunchPolicy::for_request(ctx);
            let checker = nested
                .command(std::ffi::OsStr::new("checker-probe"), &[])
                .map(|_| ());
            match (formatter, checker) {
                (Ok(()), Ok(())) => Response::success(request.id, json!({"launched": true})),
                (Err(formatter), Err(checker)) => {
                    assert_eq!(formatter, checker);
                    Response::success(request.id, json!({"refusal": formatter}))
                }
                other => panic!("one call's tools must share one policy: {other:?}"),
            }
        }
        let (_project, storage, ctx, mut identity) = worker_route(
            RouteRole::ToolProviderV1,
            subc_protocol::call_mode::CallMode::Classic,
        );
        let operator_binary = storage.path().join("operator-aft");
        std::fs::write(&operator_binary, "operator executable fixture").unwrap();
        let _launcher = crate::sandbox_spawn::install_sandbox_launcher_for_test(
            &identity.project_root,
            operator_binary.clone(),
        );
        ctx.update_config(|config| {
            config.gh_shim.binary_path = Some(operator_binary);
            config.sandbox.profile.worker = crate::config::SandboxMode::Relaxed;
        });
        let data = Arc::get_mut(&mut identity.0).unwrap();
        let mut scope = stamp("worker");
        scope.owner = Principal::Reserved {
            module_id: "prefrontal-core".into(),
        };
        data.scope = Some(scope);
        assert!(super::super::access_grants::alfonso_worker(&identity));
        let root = identity.project_root.clone();
        crate::sandbox_spawn::install_sandbox_spawn_test_seam(root.clone());
        let tool_policy_builds = || {
            crate::sandbox_spawn::sandbox_spawn_test_observations(&root)
                .into_iter()
                .filter(|observation| observation.task_kind == SandboxTaskKind::ExternalTool)
                .count()
        };
        let call = |content: &str, profile_reads: usize| -> Value {
            let run = PlainToolRun {
                speech_seed: None,
                bare_name: "write".into(),
                arguments: json!({"path": "pin.rs", "content": content}),
                format_context: Default::default(),
                tool_call_context: ToolCallContext {
                    project_root: identity.project_root.clone(),
                    session_id: Some(identity.session.clone()),
                    request_id: "tool-policy-lazy".into(),
                    diagnostics_on_edit: false,
                    preview: false,
                    edit_slot_survives: None,
                    report_registration_downgrade: false,
                    standard_edit_grammar: true,
                    disabled_tools: None,
                    worker_session: true,
                    host_tool_names: None,
                },
                dispatch: observe,
                bind_trust: identity.trust,
                identity: identity.clone(),
                request_id: "tool-policy-lazy".into(),
                ledger_key: None,
                io_budget: crate::bounded_io::Budget::after(Duration::from_secs(30)),
                approved_io_window: None,
                reply_deadline: Instant::now() + Duration::from_secs(30),
            };
            let (done, _completion) = oneshot::channel();
            let job = run.job(
                crate::access_policy::AskRoute::Lowered,
                PhaseTrace::new(Instant::now()),
                done,
            );
            // The fixture also asserts how many times a profile prepared the
            // account-home canaries (only relaxed profiles off Linux do), so a
            // call that launches nothing proves no profile was built here too.
            crate::worker_jail_canary::with_test_preparation_home(
                storage.path(),
                profile_reads,
                || job(&ctx).data,
            )
        };
        let relaxed_profile_reads = usize::from(!cfg!(target_os = "linux"));

        for _ in 0..5 {
            let data = call("pub fn edited() {}\n", 0);
            assert_eq!(data["launched"], false, "{data}");
        }
        assert_eq!(
            tool_policy_builds(),
            0,
            "worker calls that launch no tool must not build a launch policy"
        );

        let data = call("launch", relaxed_profile_reads);
        assert_eq!(data["launched"], true, "{data}");
        assert_eq!(
            tool_policy_builds(),
            1,
            "one call's formatter and checker must share one launch policy"
        );

        // A file where the task directories belong makes the policy's private
        // tool directory impossible to create, so the policy refuses.
        let task_root = storage.path().join("bash-tasks");
        if task_root.is_dir() {
            std::fs::remove_dir_all(&task_root).unwrap();
        }
        std::fs::write(&task_root, "not a directory").unwrap();
        let data = call("launch", 0);
        let built_up_front = crate::sandbox_spawn::with_authenticated_principal(
            identity.spawn_principal.clone(),
            || {
                crate::sandbox_spawn::with_worker_jail(true, || {
                    crate::log_ctx::with_session(Some(identity.session.clone()), || {
                        ToolLaunchPolicy::resolve(&ctx)
                    })
                })
            },
        );
        let expected = built_up_front
            .command(std::ffi::OsStr::new("formatter-probe"), &[])
            .map(|_| ())
            .unwrap_err();
        assert!(
            expected
                .starts_with("sandbox_unavailable: failed to allocate a private tool directory"),
            "{expected}"
        );
        assert_eq!(data["refusal"], expected, "{data}");
        crate::sandbox_spawn::clear_sandbox_spawn_test_seam(&root);
    }

    /// Fetch the worker catalog with `confine: "root"`, which saves the fence.
    async fn confine_route(
        identity: &RouteIdentity,
        ctx: &Arc<AppContext>,
        metrics: &Arc<DispatchPathMetrics>,
    ) {
        let reply = exchange_with_actor(
            json!({"op":"tool.catalog","preset":"worker","params":{"confine":"root"}}),
            identity.clone(),
            ctx.clone(),
            metrics,
            |_, _| panic!("catalog reached file dispatch"),
        )
        .await;
        assert_eq!(reply.header.ty, FrameType::Response);
        let catalog: Value = serde_json::from_slice(&reply.body).unwrap();
        let names = catalog["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, ["read", "grep", "glob", "aft_outline"]);
    }

    #[tokio::test]
    async fn confined_route_dispatch_uses_saved_policy_and_public_outline_name() {
        let _guard = EXCHANGE_LOCK.lock().await;
        let (_project, _storage, ctx, identity) = worker_route(
            RouteRole::ToolProviderV1,
            subc_protocol::call_mode::CallMode::Classic,
        );
        let metrics = Arc::new(DispatchPathMetrics::new());
        confine_route(&identity, &ctx, &metrics).await;
        let reply = exchange_with_actor(json!({"op":"tool.call", "preset":"worker", "name":"aft_outline", "arguments":{"target":"pin.rs"}}), identity.clone(), ctx.clone(), &metrics, |request, ctx| {
            assert_eq!(request.command, "outline");
            assert!(ctx.request_force_restrict(&request.id));
            crate::commands::outline::handle_outline(&request, ctx)
        }).await;
        assert_eq!(reply.header.ty, FrameType::Response);
        let result: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(result["structuredContent"]["success"], true, "{result}");
        assert!(String::from_utf8_lossy(&reply.body).contains("pinned_symbol"));
        for name in ["bash", "inspect", "write"] {
            let reply = exchange_with_actor(
                json!({"op":"tool.call", "preset":"head", "name":name,"arguments":{}}),
                identity.clone(),
                ctx.clone(),
                &metrics,
                |_, _| panic!("unoffered tool reached dispatch"),
            )
            .await;
            assert_eq!(error_body(&reply).code, "tool_not_offered_confined");
        }
    }

    /// A confined catalog offers outline as `aft_outline` on legacy routes
    /// too. Every route kind and call mode must accept that offered name and
    /// dispatch the internal `outline` command, while an unconfined worker,
    /// whose catalog never offered `aft_outline`, is still refused it.
    #[tokio::test]
    async fn confined_public_outline_name_runs_on_every_route_and_call_mode() {
        use subc_protocol::call_mode::CallMode;
        let _guard = EXCHANGE_LOCK.lock().await;
        for role in [RouteRole::Legacy, RouteRole::ToolProviderV1] {
            for mode in [CallMode::Classic, CallMode::Codemode, CallMode::Flow] {
                let label = format!("{role:?} {mode:?}");
                let (_project, _storage, ctx, identity) = worker_route(role, mode.clone());
                let metrics = Arc::new(DispatchPathMetrics::new());
                let mut body = json!({"preset":"worker", "name":"aft_outline", "arguments":{"target":"pin.rs"}});
                if role == RouteRole::ToolProviderV1 {
                    body["op"] = json!("tool.call");
                }
                ACTIONS.store(0, Ordering::SeqCst);
                let reply = exchange_with_actor(
                    body.clone(),
                    identity.clone(),
                    ctx.clone(),
                    &metrics,
                    |request, _| {
                        ACTIONS.fetch_add(1, Ordering::SeqCst);
                        Response::success(request.id, json!({}))
                    },
                )
                .await;
                let text = String::from_utf8_lossy(&reply.body);
                assert_eq!(
                    ACTIONS.load(Ordering::SeqCst),
                    0,
                    "{label} unconfined: {text}"
                );
                assert!(
                    text.contains("unknown_tool") || text.contains("call_mode_unsupported"),
                    "{label} unconfined: {text}"
                );

                confine_route(&identity, &ctx, &metrics).await;
                let reply = exchange_with_actor(
                    body,
                    identity.clone(),
                    ctx.clone(),
                    &metrics,
                    |request, ctx| {
                        assert_eq!(request.command, "outline");
                        assert!(ctx.request_force_restrict(&request.id));
                        crate::commands::outline::handle_outline(&request, ctx)
                    },
                )
                .await;
                let text = String::from_utf8_lossy(&reply.body);
                assert_eq!(reply.header.ty, FrameType::Response, "{label}: {text}");
                assert!(text.contains("pinned_symbol"), "{label}: {text}");
            }
        }
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
    async fn unknown_operations_on_tool_provider_route_return_protocol_errors() {
        let _guard = EXCHANGE_LOCK.lock().await;
        let unknown = exchange(
            json!({"op":"example.not_served","params":{}}),
            RouteRole::Legacy,
            "session",
            vec![],
        )
        .await;
        assert_eq!(unknown.header.ty, FrameType::Error);
        let error = error_body(&unknown);
        assert_eq!(error.code, "unknown_op");
        assert_eq!(
            error.message,
            "aft does not serve op \"example.not_served\" on this route"
        );

        let known = exchange(
            json!({"op":"tool.catalog","params":{}}),
            RouteRole::Legacy,
            "session",
            vec![],
        )
        .await;
        assert_eq!(known.header.ty, FrameType::Response);
    }

    async fn assert_v1_zoom_alias_dispatch_matches_batch(target: &str, arguments: Value) {
        let _guard = EXCHANGE_LOCK.lock().await;
        ACTIONS.store(0, Ordering::SeqCst);
        let canonical = exchange(
            json!({"name":"zoom", "arguments":{
                "batch":[{"target":target,"symbols":"parse"}],
                "contextLines":2,"callgraph":true
            }}),
            RouteRole::ToolProviderV1,
            "session",
            vec![],
        )
        .await;
        assert_eq!(canonical.header.ty, FrameType::Response);
        assert_eq!(ACTIONS.load(Ordering::SeqCst), 1);
        let expected = LAST_DISPATCH.lock().unwrap().clone().unwrap();
        assert_eq!(expected["command"], "zoom");

        let reply = exchange(
            json!({"name":"zoom", "arguments":arguments}),
            RouteRole::ToolProviderV1,
            "session",
            vec![],
        )
        .await;
        assert_eq!(
            reply.header.ty,
            FrameType::Response,
            "legacy zoom arguments {arguments} were refused: {}",
            String::from_utf8_lossy(&reply.body)
        );
        assert_eq!(ACTIONS.load(Ordering::SeqCst), 2);
        assert_eq!(*LAST_DISPATCH.lock().unwrap(), Some(expected));
    }

    #[tokio::test]
    async fn v1_zoom_path_alias_dispatch_matches_batch() {
        let target = std::env::current_dir().unwrap().join("zoom-alias.ts");
        let target = target.to_str().unwrap();
        assert_v1_zoom_alias_dispatch_matches_batch(
            target,
            json!({"path":target,"symbols":"parse","contextLines":2,"callgraph":true}),
        )
        .await;
    }

    #[tokio::test]
    async fn v1_zoom_targets_alias_dispatch_matches_batch() {
        let target = std::env::current_dir().unwrap().join("zoom-alias.ts");
        let target = target.to_str().unwrap();
        assert_v1_zoom_alias_dispatch_matches_batch(
            target,
            json!({"targets":[{"path":target,"symbols":"parse"}],"contextLines":2,"callgraph":true}),
        )
        .await;
    }

    #[tokio::test]
    async fn v1_zoom_mixed_batch_and_path_keeps_refusal() {
        let _guard = EXCHANGE_LOCK.lock().await;
        ACTIONS.store(0, Ordering::SeqCst);
        let reply = exchange(
            json!({"name":"zoom", "arguments":{
                "batch":[{"target":"module.ts","symbols":"parse"}],
                "path":"other.ts","symbols":"parse"
            }}),
            RouteRole::ToolProviderV1,
            "session",
            vec![],
        )
        .await;
        assert_eq!(reply.header.ty, FrameType::Error);
        assert_eq!(
            serde_json::to_value(error_body(&reply)).unwrap(),
            serde_json::to_value(errors::invalid_request(
                "path",
                "argument is absent from the served schema"
            ))
            .unwrap()
        );
        assert_eq!(ACTIONS.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn v1_zoom_alias_with_unknown_field_keeps_refusal() {
        let _guard = EXCHANGE_LOCK.lock().await;
        ACTIONS.store(0, Ordering::SeqCst);
        let reply = exchange(
            json!({"name":"zoom", "arguments":{
                "bogus":true,"path":"module.ts","symbols":"parse"
            }}),
            RouteRole::ToolProviderV1,
            "session",
            vec![],
        )
        .await;
        assert_eq!(reply.header.ty, FrameType::Error);
        assert_eq!(
            serde_json::to_value(error_body(&reply)).unwrap(),
            serde_json::to_value(errors::invalid_request(
                "bogus",
                "argument is absent from the served schema"
            ))
            .unwrap()
        );
        assert_eq!(ACTIONS.load(Ordering::SeqCst), 0);
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

#[cfg(test)]
mod ledger_terminal_tests {
    use super::*;
    use crate::db::call_ledger::Key;
    use crate::subc::{control_flags, route_key, Frame, FrameType, PersistentCancelSignal};

    /// A withdraw whose running call could not be stopped stores no answer
    /// and is refused as retryable, so a retry tries again. Once the task's
    /// record shows it stopped, the answer is stored and returned.
    #[test]
    fn withdraw_without_a_landed_kill_stores_no_answer_and_is_retryable() {
        use crate::db::call_ledger::{self as ledger, State};
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::subc::test_support::test_ctx();
        let db = std::sync::Arc::new(std::sync::Mutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        ));
        ctx.set_db(db.clone());
        let key = Key {
            carrier: "carrier".into(),
            call_key: "running".into(),
        };
        let task = "bash-0123456789abcdef";
        {
            let conn = db.lock().unwrap();
            ledger::admit(&conn, &key, "digest", None, State::DispatchStarted).unwrap();
            ledger::link_task(&conn, &key, task).unwrap();
            conn.execute(
                "INSERT INTO bash_tasks(harness,session_id,task_id,project_key,command,cwd,status,started_at,metadata) VALUES('h','s',?1,'elsewhere','sleep','cwd','running',1,'{}')",
                [task],
            )
            .unwrap();
        }
        let Ok(ledger::Withdrawal::Answer {
            body,
            kill_task: Some(kill_task),
            ..
        }) = ledger::withdraw(&db.lock().unwrap(), &key, None, 100, usize::MAX)
        else {
            panic!("a running linked call must name its task to kill");
        };
        let kill = WithdrawKill {
            key: key.clone(),
            task: kill_task,
            body,
        };
        assert!(!ctx.bash_background().holds_task(task));
        let refused = kill_withdrawn_call(&ctx, &kill).expect_err("no registry holds the task");
        assert_eq!(refused.code, "withdraw_incomplete");
        assert_eq!(refused.detail.as_ref().unwrap()["retryable"], true);
        let row = ledger::get(&db.lock().unwrap(), &key).unwrap().unwrap();
        assert!(
            row.withdraw_answer.is_none(),
            "an unstopped call has no stored answer"
        );

        db.lock()
            .unwrap()
            .execute(
                "UPDATE bash_tasks SET status='killed' WHERE task_id=?1",
                [task],
            )
            .unwrap();
        assert_eq!(kill_withdrawn_call(&ctx, &kill).unwrap(), kill.body);
        let row = ledger::get(&db.lock().unwrap(), &key).unwrap().unwrap();
        assert_eq!(row.withdraw_answer.as_deref(), Some(kill.body.as_slice()));
    }

    #[tokio::test]
    async fn daemon_edge_permission_hold_keeps_plan_arguments_owned() {
        use crate::db::call_ledger::{self as ledger, State};
        use crate::subc::*;
        let (_dir, root) = test_support::test_root("held-copy-work");
        let ctx = test_support::test_ctx();
        ctx.set_db(std::sync::Arc::new(std::sync::Mutex::new(
            crate::db::open(&_dir.path().join("aft.db")).unwrap(),
        )));
        let executor = std::sync::Arc::new(crate::executor::Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        let mut identity = test_support::route_identity(&root, "session");
        let inner = std::sync::Arc::get_mut(&mut identity.0).unwrap();
        inner.role = RouteRole::ToolProviderV1;
        inner.trust = BindTrust::Untrusted;
        inner.consumer_elicitation_capable = true;
        let key = LedgerEdge::key(&identity, "held-copy-key");
        let routes = std::collections::HashMap::from([(route_key(42, 1), identity)]);
        let arguments = json!({"command":"printf hello"});
        let body = json!({"name":"bash", "arguments":arguments, "call_key":"held-copy-key"});
        let decoded = DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame: Frame::build(
                FrameType::Request,
                control_flags(),
                42,
                1,
                7,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap(),
            phase_trace: PhaseTrace::new(std::time::Instant::now()),
        };
        let (edge, mut ready) = LedgerEdge::new();
        let (writer, _receiver) = tokio::sync::mpsc::channel(10);
        KEYED_COPY_BYTES.with(|work| work.set([0; 3]));
        assert!(edge
            .defer(
                decoded,
                &routes,
                &executor,
                &writer,
                &std::sync::Arc::new(DispatchPathMetrics::new())
            )
            .is_ok());
        let admitted = tokio::time::timeout(std::time::Duration::from_secs(5), ready.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(admitted.body().unwrap()["arguments"], arguments);
        let copies = KEYED_COPY_BYTES.with(std::cell::Cell::get);
        assert_eq!(copies[1], serde_json::to_vec(&arguments).unwrap().len());
        assert_eq!(copies[2], 0);
        let db = ctx.db().unwrap();
        assert_eq!(
            ledger::get(&db.lock().unwrap(), &key)
                .unwrap()
                .unwrap()
                .state,
            State::Prepared
        );
    }

    #[tokio::test]
    async fn daemon_edge_keyed_calls_bound_argument_and_terminal_copies() {
        use crate::db::call_ledger::{self as ledger, State};
        use crate::subc::*;
        let (_dir, root) = test_support::test_root("keyed-copy-work");
        let ctx = test_support::test_ctx();
        let path = _dir.path().join("aft.db");
        ctx.set_db(std::sync::Arc::new(std::sync::Mutex::new(
            crate::db::open(&path).unwrap(),
        )));
        let executor = std::sync::Arc::new(crate::executor::Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        let mut identity = test_support::route_identity(&root, "session");
        std::sync::Arc::get_mut(&mut identity.0).unwrap().role = RouteRole::ToolProviderV1;
        let key = LedgerEdge::key(&identity, "copy-key");
        let routes = std::collections::HashMap::from([(route_key(42, 1), identity)]);
        let (edge, mut ready) = LedgerEdge::new();
        let metrics = std::sync::Arc::new(DispatchPathMetrics::new());
        let (actual, mut receiver) = tokio::sync::mpsc::channel(10);
        let (writer, proxy) = edge.proxy(actual, executor.clone(), metrics.clone());
        let body = json!({"name":"read", "arguments":{"filePath":"r".repeat(32768)}, "call_key":"copy-key"});
        let decode = |body: &Value, corr| DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame: Frame::build(
                FrameType::Request,
                control_flags(),
                42,
                1,
                corr,
                serde_json::to_vec(body).unwrap(),
            )
            .unwrap(),
            phase_trace: PhaseTrace::new(std::time::Instant::now()),
        };
        KEYED_COPY_BYTES.with(|work| work.set([0; 3]));
        assert!(edge
            .defer(decode(&body, 7), &routes, &executor, &writer, &metrics)
            .is_ok());
        let admitted = tokio::time::timeout(std::time::Duration::from_secs(5), ready.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(admitted.frame.header.corr, 7);
        let payload = serde_json::to_vec(&json!({"text":"result".repeat(32768)})).unwrap();
        let frame = Frame::build(
            FrameType::Response,
            control_flags(),
            42,
            1,
            7,
            payload.clone(),
        )
        .unwrap();
        send_reliable_writer_frame(&writer, &metrics, frame, "copy work test")
            .await
            .unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.body(), payload);
        // After the proxy forwards the response, a separate SQLite connection
        // must see its committed ledger result. Merely scheduling that write
        // could lose an already visible result if the process crashes first.
        let conn = crate::db::open(&path).unwrap();
        let row = ledger::get(&conn, &key).unwrap().unwrap();
        assert_eq!(row.state, State::Settled);
        use sha2::Digest;
        let canonical = format!(
            r#"{{"arguments":{{"filePath":"{}"}},"name":"read","schema_pin":null}}"#,
            "r".repeat(32768)
        );
        let digest = format!("{:x}", sha2::Sha256::digest(canonical.as_bytes()));
        assert_eq!(row.digest, digest);
        assert_eq!(row.frame.unwrap().body, payload);
        let first = KEYED_COPY_BYTES.with(std::cell::Cell::get);
        eprintln!(
            "keyed first copy_bytes={first:?} response_bytes={}",
            payload.len()
        );
        let mut repeat_copies = Vec::new();
        for (label, corr, request_body) in [
            ("replay", 8, body.clone()),
            (
                "conflict",
                9,
                json!({"name":"read", "arguments":{"filePath":"different"}, "call_key":"copy-key"}),
            ),
        ] {
            KEYED_COPY_BYTES.with(|work| work.set([0; 3]));
            assert!(edge
                .defer(
                    decode(&request_body, corr),
                    &routes,
                    &executor,
                    &writer,
                    &metrics
                )
                .is_ok());
            let replay = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(replay.header.corr, corr);
            assert!(ready.try_recv().is_err());
            let copies = KEYED_COPY_BYTES.with(std::cell::Cell::get);
            eprintln!("keyed {label} copy_bytes={copies:?}");
            repeat_copies.push((
                label,
                copies,
                serde_json::to_vec(&request_body).unwrap().len(),
            ));
        }
        KEYED_COPY_BYTES.with(|work| work.set([0; 3]));
        let keyless_body = json!({"name":"read", "arguments":{"filePath":"r".repeat(32768)}});
        assert!(edge
            .defer(
                decode(&keyless_body, 10),
                &routes,
                &executor,
                &writer,
                &metrics
            )
            .is_err());
        let keyless = KEYED_COPY_BYTES.with(std::cell::Cell::get);
        eprintln!("keyless copy_bytes={keyless:?}");
        KEYED_COPY_BYTES.with(|work| work.set([0; 3]));
        let write = json!({"name":"write", "arguments":{"filePath":"file.rs", "content":"x".repeat(32768)}, "call_key":"write-copy-key"});
        assert!(edge
            .defer(decode(&write, 11), &routes, &executor, &writer, &metrics)
            .is_ok());
        tokio::time::timeout(std::time::Duration::from_secs(5), ready.recv())
            .await
            .unwrap()
            .unwrap();
        send_reliable_writer_frame(
            &writer,
            &metrics,
            Frame::build(
                FrameType::Response,
                control_flags(),
                42,
                1,
                11,
                payload.clone(),
            )
            .unwrap(),
            "write copy test",
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let write_copies = KEYED_COPY_BYTES.with(std::cell::Cell::get);
        eprintln!("keyed write copy_bytes={write_copies:?}");
        assert_eq!(
            first,
            [serde_json::to_vec(&body).unwrap().len(), 0, payload.len()]
        );
        assert_eq!(
            write_copies,
            [serde_json::to_vec(&write).unwrap().len(), 0, payload.len()]
        );
        assert_eq!(
            keyless,
            [serde_json::to_vec(&keyless_body).unwrap().len(), 0, 0],
            "keyless"
        );
        for (label, copies, envelope_bytes) in repeat_copies {
            assert_eq!(copies, [envelope_bytes, 0, 0], "{label}");
        }
        proxy.abort();
    }

    fn request() -> Frame {
        Frame::build(FrameType::Request, control_flags(), 42, 1, 7, Vec::new()).unwrap()
    }

    fn key() -> Key {
        Key {
            carrier: "first-party".into(),
            call_key: "repeat".into(),
        }
    }

    #[tokio::test]
    async fn drained_ledger_admission_never_dispatches_and_settles_not_started() {
        use crate::subc::*;
        let (_dir, root) = test_support::test_root("drained-ledger-admission");
        let ctx = test_support::test_ctx();
        ctx.set_db(std::sync::Arc::new(std::sync::Mutex::new(
            crate::db::open(&_dir.path().join("aft.db")).unwrap(),
        )));
        assert!(ctx.db().is_some());
        let executor = std::sync::Arc::new(crate::executor::Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let held = executor.submit_async(
            root.clone(),
            crate::executor::Lane::Mutating,
            "held-admission-lane".into(),
            Box::new(move |_| {
                started_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                crate::protocol::Response::success("held-admission-lane", json!({}))
            }),
        );
        started_rx.await.unwrap();
        let mut identity = test_support::route_identity(&root, "session");
        std::sync::Arc::get_mut(&mut identity.0).unwrap().role = RouteRole::ToolProviderV1;
        let routes = std::collections::HashMap::from([(route_key(42, 1), identity)]);
        let (edge, mut ready) = LedgerEdge::new();
        let decoded = DecodedFrame {
            parsed_body: Default::default(),
            confinement: None, launch_restore: None, codemode_policies: None,
            frame: Frame::build(FrameType::Request, control_flags(), 42, 1, 7, serde_json::to_vec(&json!({"name":"read", "arguments":{"filePath":"README.md"}, "call_key":"drain-key"})).unwrap()).unwrap(),
            phase_trace: PhaseTrace::new(std::time::Instant::now()),
        };
        let (writer, mut receiver) = tokio::sync::mpsc::channel(10);
        assert!(edge
            .defer(
                decoded,
                &routes,
                &executor,
                &writer,
                &std::sync::Arc::new(DispatchPathMetrics::new())
            )
            .is_ok());
        assert_eq!(edge.drain_for_module_drain().unwrap().len(), 1);
        release_tx.send(()).unwrap();
        held.await.unwrap();
        let key = LedgerEdge::key(routes.get(&route_key(42, 1)).unwrap(), "drain-key");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let row = {
                    let db = ctx.db().unwrap();
                    let conn = db.lock().unwrap();
                    crate::db::call_ledger::get(&conn, &key).unwrap()
                };
                if row.is_some_and(|row| {
                    row.state == crate::db::call_ledger::State::Settled
                        && row.outcome.as_deref() == Some("not_started")
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(ready.try_recv().is_err());
        assert!(receiver.try_recv().is_err());
        assert!(edge.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn keyed_admission_sweeps_are_coalesced_per_root_and_keep_targeted_expiry() {
        let (_dir, root) = crate::subc::test_support::test_root("sweep-rate");
        let (edge, _) = LedgerEdge::new();
        let now = std::time::Instant::now();
        assert!(edge.sweep_due(&root, now));
        assert!(!edge.sweep_due(&root, now));
        assert!(edge.sweep_due(&root, now + crate::subc::DRAIN_TICK_PERIOD));
        let source = include_str!("tool_provider.rs");
        let admission = source
            .split("let now = ledger::now_ms();")
            .nth(1)
            .unwrap()
            .split("let _ = result_tx.send")
            .next()
            .unwrap();
        assert!(admission.contains("ledger::reduce_expired"));
    }

    #[test]
    fn module_drain_counts_and_claims_ledger_admission_terminal_once() {
        let (edge, _) = LedgerEdge::new();
        let cancel = PersistentCancelSignal::new();
        let terminal_claim = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        edge.waiting.lock().unwrap().insert(
            (route_key(42, 1), 7),
            LedgerWait {
                admission: crate::subc::AdmissionWait::from_decoded(
                    &crate::subc::DecodedFrame {
                        parsed_body: Default::default(),
                        confinement: None,
                        launch_restore: None,
                        codemode_policies: None,
                        frame: request(),
                        phase_trace: crate::run_tool_call::PhaseTrace::new(
                            std::time::Instant::now(),
                        ),
                    },
                    "read",
                ),
                cancel: cancel.clone(),
                terminal_claim: terminal_claim.clone(),
            },
        );
        let census = crate::subc::drain::held_request_census(
            &std::sync::Arc::default(),
            &crate::subc::PendingSubcResponses::default(),
            &crate::subc::DispatchPathMetrics::new(),
            &Default::default(),
            &Default::default(),
            &Default::default(),
            0,
            &crate::subc::persistence::DatabaseWaits::default(),
            &edge,
        );
        assert_eq!(census.total(), 1);
        let frames = edge.drain_for_module_drain().unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].header.corr, 7);
        assert!(cancel.is_cancelled());
        assert!(terminal_claim.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!edge.has_waiting(&request()));
        assert!(edge.drain_for_module_drain().unwrap().is_empty());
    }

    #[test]
    fn missing_recorded_terminal_retires_attachment_with_unknown_retention() {
        let (edge, _) = LedgerEdge::new();
        edge.waiting.lock().unwrap().insert(
            (route_key(42, 1), 7),
            LedgerWait {
                admission: crate::subc::AdmissionWait::from_decoded(
                    &crate::subc::DecodedFrame {
                        parsed_body: Default::default(),
                        confinement: None,
                        launch_restore: None,
                        codemode_policies: None,
                        frame: request(),
                        phase_trace: crate::run_tool_call::PhaseTrace::new(
                            std::time::Instant::now(),
                        ),
                    },
                    "read",
                ),
                cancel: PersistentCancelSignal::new(),
                terminal_claim: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        let frame = edge
            .finish_recorded_result(&request(), &key(), None)
            .unwrap();
        assert!(edge.waiting.lock().unwrap().is_empty());
        assert_eq!(frame.header.corr, 7);
        assert_eq!(frame.header.ty, FrameType::Error);
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(body["code"], "result_not_retained");
        assert_eq!(body["detail"]["outcome"], "unknown");
    }

    #[test]
    fn ledger_admission_refusal_uses_the_called_tools_formatter() {
        let call =
            serde_json::from_value(serde_json::json!({"name":"search","arguments":{}})).unwrap();
        let result = ledger_refusal_result(
            &call,
            crate::protocol::Response::error("call", "database_unavailable", "ledger offline"),
            std::path::Path::new("."),
        );
        assert_eq!(
            result.text,
            "semantic_search: database_unavailable — ledger offline"
        );
    }
}

#[cfg(test)]
mod settlement_failure_tests {
    use super::*;
    use crate::subc::*;

    async fn failed_settlement(case: &str, body: Vec<u8>) -> Frame {
        let (_dir, root) = test_support::test_root("ledger-settlement-failure");
        let executor = Arc::new(Executor::new());
        let key = crate::db::call_ledger::Key {
            carrier: "fixture".into(),
            call_key: "uncertain".into(),
        };
        if case != "missing-actor" {
            let ctx = test_support::test_ctx();
            if case != "missing-db" {
                let db = Arc::new(std::sync::Mutex::new(
                    crate::db::open(&_dir.path().join("aft.db")).unwrap(),
                ));
                {
                    let conn = db.lock().unwrap();
                    if case != "absent-row" {
                        crate::db::call_ledger::admit(
                            &conn,
                            &key,
                            "digest",
                            None,
                            crate::db::call_ledger::State::DispatchStarted,
                        )
                        .unwrap();
                    }
                    if case == "already-retained" {
                        crate::db::call_ledger::settle(
                            &conn,
                            &key,
                            &crate::db::call_ledger::RecordedFrame {
                                ty: FrameType::Response,
                                body: body.clone(),
                            },
                            "ok",
                            None,
                            crate::db::call_ledger::now_ms(),
                        )
                        .unwrap();
                    }
                    if case == "sql" {
                        conn.execute_batch("CREATE TRIGGER refuse_settlement BEFORE UPDATE ON call_ledger BEGIN SELECT RAISE(ABORT, 'settlement fixture failure'); END;").unwrap();
                    }
                }
                if case == "poisoned-db" {
                    let poison = db.clone();
                    let _ = std::thread::spawn(move || {
                        let _guard = poison.lock().unwrap();
                        panic!("poison settlement fixture");
                    })
                    .join();
                }
                ctx.set_db(db);
            }
            executor.register_actor(root.clone(), ctx);
        }
        let (edge, _) = LedgerEdge::new();
        let route = route_key(42, 3);
        edge.calls.lock().unwrap().insert(
            (route, 7),
            LedgerCall {
                root,
                key,
                shell: false,
                detached: false,
                withdrawn: None,
                expires_at: Instant::now() + Duration::from_secs(60),
            },
        );
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (actual, mut receiver) = tokio::sync::mpsc::channel(4);
        let (writer, proxy) = edge.proxy(actual, executor.clone(), metrics.clone());
        let original = Frame::build(FrameType::Response, control_flags(), 42, 3, 7, body).unwrap();
        send_reliable_writer_frame(&writer, &metrics, original, "settlement fixture")
            .await
            .unwrap();
        drop(writer);
        proxy.await.unwrap();
        let terminal = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .expect("failed settlement must still answer the original caller");
        assert_eq!(
            terminal.header.ty,
            if case == "already-retained" {
                FrameType::Response
            } else {
                FrameType::Error
            },
            "{case}"
        );
        assert_eq!(
            (
                terminal.header.channel,
                terminal.header.epoch,
                terminal.header.corr
            ),
            (42, 3, 7)
        );
        assert_eq!(terminal.header.flags, control_flags());
        assert_eq!(metrics.writer_queued.load(Ordering::Relaxed), 1);
        decrement_counted_channel(&metrics.writer_queued);
        assert_eq!(metrics.writer_queued.load(Ordering::Relaxed), 0);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .unwrap()
                .is_none(),
            "one original request has one terminal"
        );
        terminal.frame
    }

    async fn check_failure(case: &str) {
        let body = serde_json::to_vec(&serde_json::json!({"isError":false,"content":[{"type":"text","text":"already produced"}]})).unwrap();
        let terminal = failed_settlement(case, body.clone()).await;
        let error: Value = serde_json::from_slice(&terminal.body).unwrap();
        assert_eq!(error["code"], "outcome_unknown");
        assert!(error["message"]
            .as_str()
            .unwrap()
            .contains("replay is not guaranteed"));
        assert_eq!(
            error["detail"]["original_result_excerpt"],
            String::from_utf8(body).unwrap()
        );
        assert_eq!(error["detail"]["original_result_truncated"], false);
    }

    #[tokio::test]
    async fn missing_actor_settlement_answers_once_with_uncertain_durable_outcome() {
        check_failure("missing-actor").await;
    }
    #[tokio::test]
    async fn sql_settlement_failure_answers_once_with_uncertain_durable_outcome() {
        check_failure("sql").await;
    }
    #[tokio::test]
    async fn missing_database_settlement_does_not_claim_replayable_success() {
        check_failure("missing-db").await;
    }
    #[tokio::test]
    async fn poisoned_database_settlement_does_not_claim_replayable_success() {
        check_failure("poisoned-db").await;
    }

    #[tokio::test]
    async fn absent_ledger_row_does_not_claim_successful_settlement() {
        check_failure("absent-row").await;
    }

    #[tokio::test]
    async fn already_retained_matching_terminal_is_forwarded_without_uncertainty() {
        let body = serde_json::to_vec(&serde_json::json!({"isError":false,"content":[{"type":"text","text":"already produced"}]})).unwrap();
        let terminal = failed_settlement("already-retained", body.clone()).await;
        assert_eq!(terminal.body, body);
    }

    #[tokio::test]
    async fn uncertain_settlement_result_detail_is_bounded_and_disclosed() {
        let body = serde_json::to_vec(&serde_json::json!({"isError":false,"content":[{"type":"text","text":"é".repeat(16384)}]})).unwrap();
        let terminal = failed_settlement("sql", body).await;
        let error: Value = serde_json::from_slice(&terminal.body).unwrap();
        let excerpt = error["detail"]["original_result_excerpt"].as_str().unwrap();
        assert!(excerpt.len() <= 8192);
        assert_eq!(error["detail"]["original_result_truncated"], true);
    }
}

#[cfg(test)]
mod authorization_failure_tests {
    use super::*;
    use crate::subc::*;

    fn pending_ask(root: &ProjectRootId) -> PendingBashAsk {
        let identity =
            test_support::route_identity_with_trust(root, "authorization", BindTrust::Untrusted);
        PendingBashAsk {
            handles_progress: false,
            route: route_key(42, 3),
            tool_corr: 78,
            tool_flags: control_flags(),
            tool_ver: PROTOCOL_VERSION,
            root: root.clone(),
            project_root: root.as_path().to_path_buf(),
            session_id: identity.session.clone(),
            spawn_principal: identity.spawn_principal.clone(),
            edit_slot_survives: None,
            call_key: Some("authorization".into()),
            request_id: "authorization".into(),
            arguments: json!({"command":"printf must-not-run"}),
            format_context: crate::subc_format::FormatContext::default(),
            cancel: bash::BashWaitCancel {
                speech_seed: None,
                run_scope: None,
                scope: None,
                late_results: false,
                ledger_carrier: None,
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: drain::ModuleDrainWindow::default(),
            },
            grants: Vec::new(),
            repeat: None,
            worker_session: false,
            worker_preset: false,
            reply_budget: crate::commands::blocking_wait::ReplyBudget::new(Instant::now(), None),
            asked_at: Instant::now(),
            expires_at: Instant::now() + Duration::from_secs(60),
        }
    }

    async fn authorization_failure(case: &str) {
        let (_dir, root) = test_support::test_root("authorization-failure");
        let executor = Arc::new(Executor::new());
        let ctx = test_support::test_ctx();
        let key = crate::db::call_ledger::Key {
            carrier: "fixture".into(),
            call_key: "authorization".into(),
        };
        if case != "missing-db" {
            let db = Arc::new(std::sync::Mutex::new(
                crate::db::open(&_dir.path().join("aft.db")).unwrap(),
            ));
            {
                let conn = db.lock().unwrap();
                crate::db::call_ledger::admit(
                    &conn,
                    &key,
                    "digest",
                    None,
                    crate::db::call_ledger::State::Prepared,
                )
                .unwrap();
                let state = if case == "first-transition" {
                    "Authorized"
                } else {
                    "DispatchStarted"
                };
                conn.execute_batch(&format!("CREATE TRIGGER refuse_authorization BEFORE UPDATE ON call_ledger WHEN NEW.state='{state}' BEGIN SELECT RAISE(ABORT, 'authorization fixture failure'); END;")).unwrap();
            }
            ctx.set_db(db);
        }
        executor.register_actor(root.clone(), ctx);
        let route = route_key(42, 3);
        let (edge, mut ready) = LedgerEdge::new();
        edge.calls.lock().unwrap().insert(
            (route, 78),
            LedgerCall {
                root: root.clone(),
                key,
                shell: true,
                detached: false,
                withdrawn: None,
                expires_at: Instant::now() + Duration::from_secs(60),
            },
        );
        let reverse = ReverseCorrKey { route, corr: 100 };
        let mut asks = HashMap::from([(reverse, pending_ask(&root))]);
        let allow = || DecodedFrame {
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            parsed_body: Default::default(),
            frame: Frame::build(
                FrameType::Response,
                control_flags(),
                42,
                3,
                100,
                br#"{"decision":"allow"}"#.to_vec(),
            )
            .unwrap(),
            phase_trace: PhaseTrace::new(Instant::now()),
        };
        assert!(edge.authorize(allow(), &asks, &executor).is_ok());
        assert!(edge.authorize(allow(), &asks, &executor).is_ok());
        let failure = tokio::time::timeout(Duration::from_secs(1), ready.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failure.frame.header.ty, FrameType::Error);
        assert_eq!(failure.frame.header.corr, 100);
        let error: Value = serde_json::from_slice(&failure.frame.body).unwrap();
        assert_eq!(error["code"], "database_unavailable");
        let identity =
            test_support::route_identity_with_trust(&root, "authorization", BindTrust::Untrusted);
        let routes = HashMap::from([(route, identity)]);
        let (writer, mut replies) = tokio::sync::mpsc::channel(8);
        let (bash_tx, mut bash_rx) = tokio::sync::mpsc::channel(8);
        let (touch_tx, _) = tokio::sync::mpsc::channel(8);
        let metrics = Arc::new(DispatchPathMetrics::new());
        let mut live = HashMap::new();
        let mut cancels = HashMap::new();
        let shutdown = Arc::new(Notify::new());
        let dispatch: DispatchFn =
            |_, _| panic!("failed authorization must never dispatch a command");
        handle_bash_elicitation_reply(
            &writer,
            &failure.frame,
            true,
            &mut asks,
            &routes,
            &mut live,
            &executor,
            &shutdown,
            &bash_tx,
            &touch_tx,
            &metrics,
            &mut cancels,
            dispatch,
        )
        .await
        .unwrap();
        assert!(asks.is_empty());
        let reply = replies.recv().await.unwrap();
        assert_eq!(reply.header.corr, 78);
        let body: Value = serde_json::from_slice(reply.body()).unwrap();
        assert_eq!(body["isError"], true);
        assert!(body["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("could not be durably recorded"));
        handle_bash_elicitation_reply(
            &writer,
            &failure.frame,
            true,
            &mut asks,
            &routes,
            &mut live,
            &executor,
            &shutdown,
            &bash_tx,
            &touch_tx,
            &metrics,
            &mut cancels,
            dispatch,
        )
        .await
        .unwrap();
        assert!(replies.try_recv().is_err());
        assert!(bash_rx.try_recv().is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), ready.recv())
                .await
                .is_err(),
            "duplicate allows have one authorization completion"
        );
    }

    #[tokio::test]
    async fn failed_prepared_authorization_resolves_one_ask_without_execution() {
        authorization_failure("first-transition").await;
    }
    #[tokio::test]
    async fn failed_dispatch_transition_resolves_one_ask_without_execution() {
        authorization_failure("second-transition").await;
    }
    #[tokio::test]
    async fn missing_authorization_database_resolves_one_ask_without_execution() {
        authorization_failure("missing-db").await;
    }
}
