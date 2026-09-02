//! Credential-free routing shim for the `gh` argv[0] entry point.
//!
//! The shim is intentionally a small process boundary: R1/R2 and declared
//! mechanical R3 operations replace this process with upstream `gh`, while the
//! governed path is the only path that interprets a declared command shape.
//!
//! Governed invocations are executed seam-side by the route holder under full
//! GitHub App installation tokens held in custody; the shim carries a routed
//! request one way and a result-or-refusal the other, and holds no token in
//! either direction. Operation gating is holder-side classification over the
//! routed request, not a property of any token the shim can see or hold.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use subc_client_rs::{CallOptions, CloseRouteOptions, ConsumerOptions, SubcConsumer};
use subc_protocol::manifest::ProviderRole;

use crate::db::github_read_cache::{invalidate_github_read_cache_resource, GithubReadResourceKind};
use subc_protocol::{BindIdentity, RouteTarget};

pub const SCHEMA_FLOOR: u64 = 1;
/// Envelope version that carries the manifest as exact signed bytes. Envelope
/// v1 re-serialized the parsed manifest at verify time; envelope v2 verifies
/// the distributed bytes themselves (see the verifier-site contract).
pub const ENVELOPE_VERSION: u64 = 2;
pub const REFUSAL_EXIT_STATUS: i32 = 86;
const UPSTREAM_FAILURE_EXIT_STATUS: i32 = 1;
const DISCOVERY_BUDGET: Duration = Duration::from_millis(150);
const DISCOVERY_CACHE_TTL: Duration = Duration::from_secs(15);
/// Clock skew tolerated before a manifest's signed issue time counts as being
/// in the future and therefore invalid.
const ISSUED_AT_FUTURE_SKEW: Duration = Duration::from_secs(300);
const ROUTING_OPERATION: &str = "gh.route";
const ROUTING_HOLDER_MODULE_ID: &str = "prefrontal-core";
const MANIFEST_ARTIFACT_ID: &str = "gh-routing-manifest";
const V1_GOVERNED_TUPLES: &[&str] = &["issue comment", "pr comment", "pr review", "issue reaction"];
const V1_ADMIN_TUPLES: &[&str] = &["issue close", "pr close", "pr merge", "release create"];
const V9_ADMIN_TUPLES: &[&str] = &["repo edit", "run delete"];
// These v10 tuples are explicitly reviewed for the operator-only bypass and
// still require a matching signed manifest declaration. A rerun is
// administration rather than governed bot speech: it has no public attribution
// surface, while granting speech Apps actions:write would widen the compromise
// surface. `run cancel` remains deliberately absent because it is destructive
// and rarely needed, so the operator bypass cannot enable it by accident.
const V10_ADMIN_TUPLES: &[&str] = &["workflow run", "run rerun"];
// The v10 manifest version is the first version whose code-side allowlist
// permits these native comment mutations. The allowlist covers only the exact
// flag variants below and does not broaden raw API writes.
const V10_EDIT_LAST_TUPLES: &[&str] = &["issue comment", "pr comment"];
const READ_ONLY_ACTION_TUPLES: &[&str] = &[
    "run view",
    "run list",
    "run watch",
    "workflow view",
    "workflow list",
];
const RESERVED_SELF_REPORT: &[&str] = &["--status", "--shim-version"];
const CO_AUTHOR_LINE_REPORT: &str = "--co-author-line";
const GOVERNANCE_UNAVAILABLE_TEXT: &str = "the governance daemon is unreachable and this repository's actions are identity-governed; retry after the daemon returns";
const UNTRUSTED_MANIFEST_KEY_STEERING: &str = "the manifest may be newer than this aft build's trust set - update aft, or install a manifest signed by a trusted key";
const PRE_PROVENANCE_RECORD: &str = "unrecorded (pre-provenance record)";

/// The only shim-originated refusal identifiers. Keep this enumeration closed:
/// callers must parse these identifiers rather than human prose.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefusalCode {
    Unclassified,
    AdminTier,
    ManifestBelowFloor,
    ManifestRegressed,
    SeamSchemaMismatch,
    UnboundIdentity,
    BypassAuditUnavailable,
    NoRealGh,
    GovernanceUnavailable,
    SeamUnavailable,
    SeamRefusal,
}

impl RefusalCode {
    pub const ALL: [Self; 11] = [
        Self::Unclassified,
        Self::AdminTier,
        Self::ManifestBelowFloor,
        Self::ManifestRegressed,
        Self::SeamSchemaMismatch,
        Self::UnboundIdentity,
        Self::BypassAuditUnavailable,
        Self::NoRealGh,
        Self::GovernanceUnavailable,
        Self::SeamUnavailable,
        Self::SeamRefusal,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unclassified => "gh_shim_unclassified",
            Self::AdminTier => "gh_shim_admin_tier",
            Self::ManifestBelowFloor => "gh_shim_manifest_below_floor",
            Self::ManifestRegressed => "gh_shim_manifest_regressed",
            Self::SeamSchemaMismatch => "gh_shim_seam_schema_mismatch",
            Self::UnboundIdentity => "gh_shim_unbound_identity",
            Self::BypassAuditUnavailable => "gh_shim_bypass_audit_unavailable",
            Self::NoRealGh => "gh_shim_no_real_gh",
            Self::GovernanceUnavailable => "gh_shim_governance_unavailable",
            Self::SeamUnavailable => "gh_shim_seam_unavailable",
            Self::SeamRefusal => "gh_shim_seam_refusal",
        }
    }
}

/// Offline self-report uses diagnostic identifiers distinct from invocation
/// refusals. A report can therefore describe historical local-state trouble
/// without pretending that an upstream `gh` invocation was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelfReportDiagnostic {
    ManifestUnavailable,
    ManifestInvalid,
    ManifestBelowFloor,
    ManifestRegressed,
    ManifestRollback,
    RungUnavailable,
}

impl SelfReportDiagnostic {
    pub const ALL: [Self; 6] = [
        Self::ManifestUnavailable,
        Self::ManifestInvalid,
        Self::ManifestBelowFloor,
        Self::ManifestRegressed,
        Self::ManifestRollback,
        Self::RungUnavailable,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ManifestUnavailable => "gh_shim_status_manifest_unavailable",
            Self::ManifestInvalid => "gh_shim_status_manifest_invalid",
            Self::ManifestBelowFloor => "gh_shim_status_manifest_below_floor",
            Self::ManifestRegressed => "gh_shim_status_manifest_regressed",
            Self::ManifestRollback => "gh_shim_status_manifest_rollback",
            Self::RungUnavailable => "gh_shim_status_rung_unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Mechanical,
    Governed,
    Admin,
}

impl Tier {
    fn rank(self) -> u8 {
        match self {
            Self::Mechanical => 0,
            Self::Governed => 1,
            Self::Admin => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Rung {
    R1,
    R2,
    R3,
}

impl Rung {
    const fn label(self) -> &'static str {
        match self {
            Self::R1 => "R1",
            Self::R2 => "R2",
            Self::R3 => "R3",
        }
    }
}

/// Return true when the process was invoked through the `gh` symlink or the
/// explicit `aft gh-shim` development entry point. This is public so the binary
/// can perform it before its own global `--version` and `--subc` scans.
pub fn is_shim_invocation(program: &OsStr, args: &[OsString]) -> bool {
    Path::new(program)
        .file_name()
        .is_some_and(|name| name == OsStr::new("gh"))
        || args.first().is_some_and(|arg| arg == OsStr::new("gh-shim"))
}

pub fn is_shim_invocation_from_env() -> bool {
    let mut argv = std::env::args_os();
    let Some(program) = argv.next() else {
        return false;
    };
    is_shim_invocation(&program, &argv.collect::<Vec<_>>())
}

/// Execute the shim for either supported entry form. This intentionally runs
/// before logging initialization so delegating invocations cannot add shim bytes
/// to upstream stderr.
pub fn run_from_env() -> i32 {
    let mut argv = std::env::args_os();
    let Some(program) = argv.next() else {
        return refuse(RefusalCode::NoRealGh, "the executing image was unavailable");
    };
    let raw_args = argv.collect::<Vec<_>>();
    let shim_args = if Path::new(&program)
        .file_name()
        .is_some_and(|name| name == OsStr::new("gh"))
    {
        raw_args
    } else {
        raw_args.into_iter().skip(1).collect()
    };
    run(&shim_args)
}

fn run(args: &[OsString]) -> i32 {
    let paths = StatePaths::from_process();
    if args.first().and_then(|arg| arg.to_str()) == Some(CO_AUTHOR_LINE_REPORT) {
        if let Some(line) = co_author_line(&paths) {
            println!("{line}");
        }
        return 0;
    }
    if is_reserved_self_report(args) {
        print_self_report(&paths);
        return 0;
    }

    let now = unix_seconds();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Presence-based regressed-manifest arm. Decide from the installed artifact
    // BEFORE any rung probe so a governed refusal never depends on daemon
    // reachability: a validation failure after a prior valid manifest makes
    // governed/admin tuples refuse while mechanical operations pass through.
    let initial_manifest = resolve_manifest(&paths, now);
    let invalid_manifest_problem = initial_manifest.invalid_problem().cloned();
    if let ManifestResolution::Regressed { manifest, problem } = &initial_manifest {
        return match regressed_disposition(args, manifest, current_platform(), problem) {
            RegressedDisposition::Passthrough => {
                delegate_after_invalid_manifest_notice(args, problem)
            }
            RegressedDisposition::Refuse { code, text } => refuse(code, &text),
        };
    }

    let determination = determine_rung(&paths, &cwd, now);
    if determination.record.rung != Rung::R3 {
        let disposition = match resolve_manifest(&paths, now) {
            ManifestResolution::Active(manifest) => non_r3_governance_disposition(
                &cwd,
                &determination,
                args,
                &manifest,
                current_platform(),
            ),
            ManifestResolution::Regressed { .. }
            | ManifestResolution::Invalid(_)
            | ManifestResolution::Dormant => GovernanceDisposition::Delegate,
        };
        return match disposition {
            GovernanceDisposition::Unavailable(agent_binding) => {
                refuse_governance_unavailable(&paths, &agent_binding, now)
            }
            GovernanceDisposition::Unclassified { manifest_version } => refuse(
                RefusalCode::Unclassified,
                &format!(
                    "no manifest declaration for this invocation (manifest {manifest_version})"
                ),
            ),
            GovernanceDisposition::Delegate | GovernanceDisposition::Ready => {
                match invalid_manifest_problem.as_ref() {
                    Some(problem) => delegate_after_invalid_manifest_notice(args, problem),
                    None => delegate(args),
                }
            }
        };
    }

    // A valid manifest gates R3 both during fresh discovery and when a cached
    // R3 determination is reused. If it disappears or fails validation between
    // those two moments, the whole invocation falls back to R2 passthrough
    // instead of a classification-shaped refusal.
    let manifest = match resolve_manifest(&paths, now) {
        ManifestResolution::Active(manifest) => manifest,
        ManifestResolution::Regressed { manifest, problem } => {
            return match regressed_disposition(args, &manifest, current_platform(), &problem) {
                RegressedDisposition::Passthrough => {
                    delegate_after_invalid_manifest_notice(args, &problem)
                }
                RegressedDisposition::Refuse { code, text } => refuse(code, &text),
            }
        }
        ManifestResolution::Invalid(problem) => {
            return delegate_after_invalid_manifest_notice(args, &problem)
        }
        ManifestResolution::Dormant => return delegate(args),
    };
    let Some(agent_binding) = resolved_agent_binding(&manifest, &cwd) else {
        return delegate(args);
    };

    match classify(args, &manifest, current_platform()) {
        Classification::Mechanical => delegate(args),
        Classification::Admin { tuple } => {
            if std::env::var_os("GH_SHIM_BYPASS").as_deref() == Some(OsStr::new("operator")) {
                let repository = explicit_repo(args).or_else(infer_repository_from_git);
                if let Err(error) = append_bypass_audit(&paths, &tuple, repository.as_deref(), now)
                {
                    return refuse(
                        RefusalCode::BypassAuditUnavailable,
                        &format!("operator bypass audit could not be appended: {error}"),
                    );
                }
                delegate(args)
            } else {
                refuse(
                    RefusalCode::AdminTier,
                    "this action requires GH_SHIM_BYPASS=operator",
                )
            }
        }
        Classification::Governed { tuple, canonical } => {
            let request =
                match canonicalize_governed(args, &tuple, &canonical, manifest.manifest_version) {
                    Ok(request) => request,
                    Err(error) => return refuse_governed_canonicalization(&error),
                };
            let mutation = GithubReadMutation::from_governed_request(&request);
            let outcome =
                route_governed(&paths, &determination.record, &agent_binding, request, now);
            invalidate_successful_github_read_mutation(mutation.as_ref(), &outcome);
            governed_outcome_status(&paths, &agent_binding, now, outcome)
        }
        Classification::Unclassified => refuse(
            RefusalCode::Unclassified,
            &format!(
                "no manifest declaration for this invocation (manifest {})",
                manifest.manifest_version
            ),
        ),
    }
}

fn refuse_governed_canonicalization(error: &str) -> i32 {
    refuse(RefusalCode::Unclassified, error)
}

fn governed_outcome_status(
    paths: &StatePaths,
    agent_binding: &AgentBinding,
    now: u64,
    outcome: RouteOutcome,
) -> i32 {
    match outcome {
        RouteOutcome::Result(output) => {
            print!("{output}");
            0
        }
        RouteOutcome::UpstreamError(body) => {
            eprintln!("{body}");
            UPSTREAM_FAILURE_EXIT_STATUS
        }
        RouteOutcome::Refusal(code) => refuse(RefusalCode::SeamRefusal, &seam_refusal_text(&code)),
        RouteOutcome::UnboundIdentity => refuse(
            RefusalCode::UnboundIdentity,
            "the project binding was unavailable at route time",
        ),
        RouteOutcome::SchemaMismatch(message) => refuse(RefusalCode::SeamSchemaMismatch, &message),
        RouteOutcome::GovernanceUnavailable => {
            refuse_governance_unavailable(paths, agent_binding, now)
        }
        RouteOutcome::Unavailable(message) => refuse(RefusalCode::SeamUnavailable, &message),
    }
}

fn seam_refusal_text(code: &str) -> String {
    format!("governance seam refused the action: {code}")
}

fn is_reserved_self_report(args: &[OsString]) -> bool {
    args.first()
        .and_then(|arg| arg.to_str())
        .is_some_and(|arg| RESERVED_SELF_REPORT.contains(&arg))
}

#[derive(Clone, Debug)]
struct StatePaths {
    root: PathBuf,
    manifest: PathBuf,
    rung: PathBuf,
    bypass_audit: PathBuf,
    unexpected_gh_route_advertisers: PathBuf,
    seam_state: PathBuf,
    last_valid_manifest: PathBuf,
    version_high_water: PathBuf,
    numeric_ids: PathBuf,
}

impl StatePaths {
    fn from_process() -> Self {
        let root = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(|home| PathBuf::from(home).join(".local/state"))
            })
            .unwrap_or_else(|| std::env::temp_dir())
            .join("cortexkit")
            .join("aft")
            .join("gh-shim");
        Self::from_root(root)
    }

    fn from_root(root: PathBuf) -> Self {
        Self {
            manifest: root.join("gh-routing-manifest.json"),
            rung: root.join("rung-cache.json"),
            bypass_audit: root.join("operator-bypass.jsonl"),
            unexpected_gh_route_advertisers: root.join("unexpected-gh-route-advertisers.json"),
            seam_state: root.join("seam-state.json"),
            last_valid_manifest: root.join("last-valid-manifest.json"),
            version_high_water: root.join("manifest-version-high-water.json"),
            numeric_ids: root.join("numeric-ids.json"),
            root,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RungRecord {
    rung: Rung,
    as_of_unix_secs: u64,
    #[serde(default)]
    inputs: BTreeMap<String, String>,
    #[serde(default)]
    manifest_version: Option<u64>,
    #[serde(default)]
    recorded_by_image_path: Option<String>,
    #[serde(default)]
    recorded_by_version: Option<String>,
    #[serde(default)]
    recorded_by_repo_key: Option<String>,
}

#[derive(Clone, Debug)]
struct RungRecordProvenance {
    image_path: String,
    version: String,
    repo_key: String,
}

impl RungRecordProvenance {
    fn for_cwd(cwd: &Path) -> Self {
        let project_root = project_root_for(cwd);
        Self {
            image_path: executing_image().to_string_lossy().into_owned(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            repo_key: repository_key_from_origin(&project_root)
                .unwrap_or_else(|| "unresolved (no GitHub origin)".to_string()),
        }
    }
}

impl RungRecord {
    fn fresh_at(&self, now: u64) -> bool {
        now.saturating_sub(self.as_of_unix_secs) < DISCOVERY_CACHE_TTL.as_secs()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
enum R1Reason {
    DisabledByConfig,
    AbsentOrUnparseable,
    Unreachable,
    DiscoveryBudgetExhausted,
    #[cfg(test)]
    Count,
}

impl R1Reason {
    #[cfg(test)]
    const ALL: [Self; Self::Count as usize] = [
        Self::DisabledByConfig,
        Self::AbsentOrUnparseable,
        Self::Unreachable,
        Self::DiscoveryBudgetExhausted,
    ];

    const fn diagnostic(self) -> &'static str {
        match self {
            Self::DisabledByConfig => "disabled_by_config",
            Self::AbsentOrUnparseable => "absent_or_unparseable",
            Self::Unreachable => "unreachable",
            Self::DiscoveryBudgetExhausted => "discovery_budget_exhausted",
            #[cfg(test)]
            Self::Count => unreachable!(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
enum R2Reason {
    ManifestUnavailable,
    AgentBindingUnavailable,
    AgentCredentialsPresent,
    DaemonUnreachable,
    CatalogGhRouteAbsent,
    GhRouteHolderUnbound,
    #[cfg(test)]
    Count,
}

impl R2Reason {
    #[cfg(test)]
    const ALL: [Self; Self::Count as usize] = [
        Self::ManifestUnavailable,
        Self::AgentBindingUnavailable,
        Self::AgentCredentialsPresent,
        Self::DaemonUnreachable,
        Self::CatalogGhRouteAbsent,
        Self::GhRouteHolderUnbound,
    ];

    const fn diagnostic(self) -> &'static str {
        match self {
            Self::ManifestUnavailable => "manifest_unavailable",
            Self::AgentBindingUnavailable => "agent_binding_unavailable",
            Self::AgentCredentialsPresent => "agent_credentials_present",
            Self::DaemonUnreachable => "daemon_unreachable",
            Self::CatalogGhRouteAbsent => "catalog_gh_route_absent",
            Self::GhRouteHolderUnbound => "gh_route_holder_unbound",
            #[cfg(test)]
            Self::Count => unreachable!(),
        }
    }
}

#[derive(Clone, Debug)]
struct RungDetermination {
    record: RungRecord,
    operator_disabled: bool,
}

impl RungDetermination {
    fn r1(now: u64, reason: R1Reason) -> Self {
        Self {
            record: RungRecord {
                rung: Rung::R1,
                as_of_unix_secs: now,
                inputs: BTreeMap::from([(
                    "connection_file".to_string(),
                    reason.diagnostic().to_string(),
                )]),
                manifest_version: None,
                recorded_by_image_path: None,
                recorded_by_version: None,
                recorded_by_repo_key: None,
            },
            operator_disabled: reason == R1Reason::DisabledByConfig,
        }
    }

    fn r2(
        now: u64,
        reason: R2Reason,
        manifest_version: Option<u64>,
        provenance: &RungRecordProvenance,
    ) -> Self {
        Self {
            record: RungRecord {
                rung: Rung::R2,
                as_of_unix_secs: now,
                inputs: BTreeMap::from([
                    ("connection_file".to_string(), "ready".to_string()),
                    (reason.diagnostic().to_string(), "failed".to_string()),
                ]),
                manifest_version,
                recorded_by_image_path: Some(provenance.image_path.clone()),
                recorded_by_version: Some(provenance.version.clone()),
                recorded_by_repo_key: Some(provenance.repo_key.clone()),
            },
            operator_disabled: false,
        }
    }

    fn r3(now: u64, manifest_version: u64, provenance: &RungRecordProvenance) -> Self {
        Self {
            record: RungRecord {
                rung: Rung::R3,
                as_of_unix_secs: now,
                inputs: BTreeMap::from([
                    ("connection_file".to_string(), "ready".to_string()),
                    ("catalog_gh_route".to_string(), "ready".to_string()),
                    ("agent_binding".to_string(), "ready".to_string()),
                    ("manifest".to_string(), "ready".to_string()),
                    (
                        "agent_credentials_present".to_string(),
                        "absent".to_string(),
                    ),
                ]),
                manifest_version: Some(manifest_version),
                recorded_by_image_path: Some(provenance.image_path.clone()),
                recorded_by_version: Some(provenance.version.clone()),
                recorded_by_repo_key: Some(provenance.repo_key.clone()),
            },
            operator_disabled: false,
        }
    }

    fn cached(record: RungRecord) -> Self {
        Self {
            record,
            operator_disabled: false,
        }
    }
}

#[derive(Debug)]
enum GovernanceDisposition {
    Delegate,
    Ready,
    Unavailable(AgentBinding),
    Unclassified { manifest_version: u64 },
}

fn structural_governance_disposition(
    determination: &RungDetermination,
    classification: &Classification,
    agent_binding: Option<AgentBinding>,
    manifest_version: u64,
) -> GovernanceDisposition {
    if determination.operator_disabled || matches!(classification, Classification::Mechanical) {
        return GovernanceDisposition::Delegate;
    }
    let Some(agent_binding) = agent_binding else {
        return GovernanceDisposition::Delegate;
    };
    if determination.record.rung == Rung::R3 {
        return GovernanceDisposition::Ready;
    }

    match classification {
        Classification::Governed { .. } | Classification::Admin { .. } => {
            GovernanceDisposition::Unavailable(agent_binding)
        }
        Classification::Unclassified => GovernanceDisposition::Unclassified { manifest_version },
        Classification::Mechanical => GovernanceDisposition::Delegate,
    }
}

fn non_r3_governance_disposition(
    cwd: &Path,
    determination: &RungDetermination,
    args: &[OsString],
    manifest: &Manifest,
    platform: &str,
) -> GovernanceDisposition {
    if determination.operator_disabled {
        return GovernanceDisposition::Delegate;
    }

    let classification = classify(args, manifest, platform);
    if matches!(classification, Classification::Mechanical) {
        return GovernanceDisposition::Delegate;
    }

    // Binding resolution runs `git` to inspect the origin. Classify first so
    // unmanifested public repositories keep the R1 fast path for mechanical
    // reads; only a verb that could refuse pays the subprocess latency.
    let agent_binding = resolved_agent_binding(manifest, cwd);
    structural_governance_disposition(
        determination,
        &classification,
        agent_binding,
        manifest.manifest_version,
    )
}

fn determine_rung(paths: &StatePaths, cwd: &Path, now: u64) -> RungDetermination {
    // The budget starts before the config read and connection-file stat. This
    // keeps a slow filesystem from silently extending discovery beyond 150ms.
    let deadline = std::time::Instant::now() + DISCOVERY_BUDGET;
    let config_doc = read_user_config_doc();
    determine_rung_from_doc(paths, cwd, now, deadline, config_doc.as_deref())
}

/// Pure rung determination over the user config document. `config_doc` is the
/// raw user-tier `aft.jsonc` text (already read by the caller); `None` means the
/// config file was absent or unreadable. Splitting the config read from the
/// decision keeps the disabled short-circuit testable without mutating process
/// env (which races under the parallel test runner).
fn determine_rung_from_doc(
    paths: &StatePaths,
    cwd: &Path,
    now: u64,
    deadline: std::time::Instant,
    config_doc: Option<&str>,
) -> RungDetermination {
    // Operator hard-off: when the user disables the shim, short-circuit to
    // byte-transparent passthrough (R1) before any daemon/catalog probing, so a
    // disabled shim performs no governance-daemon or catalog traffic. Explicit
    // operator intent beats manifest governance; this in-memory bit is deliberately
    // not inferred from the diagnostic reason string later in dispatch.
    if gh_shim_enabled_from_config_doc(config_doc.unwrap_or("")) == Some(false) {
        return RungDetermination::r1(now, R1Reason::DisabledByConfig);
    }

    let Some(connection_file) = connection_file_from_config_doc(config_doc.unwrap_or("")) else {
        // R1 has no daemon dial and no durable determination write.
        return RungDetermination::r1(now, R1Reason::AbsentOrUnparseable);
    };
    if !connection_file.is_file() {
        return RungDetermination::r1(now, R1Reason::Unreachable);
    }

    let cached = load_rung_record(paths);
    if std::time::Instant::now() >= deadline {
        return cached
            .filter(|record| record.fresh_at(now))
            .map(RungDetermination::cached)
            .unwrap_or_else(|| RungDetermination::r1(now, R1Reason::DiscoveryBudgetExhausted));
    }
    if let Some(record) = cached.as_ref().filter(|record| record.fresh_at(now)) {
        if record.rung != Rung::R3
            || resolve_manifest(paths, now)
                .manifest()
                .and_then(|manifest| resolved_agent_binding(manifest, cwd))
                .is_some()
        {
            return RungDetermination::cached(record.clone());
        }
    }

    let provenance = RungRecordProvenance::for_cwd(cwd);
    // The signed manifest supplies the binding before the probe opens a route, so
    // rate accounting and audit records use the same agent session on every run.
    // A failed validation does not supply a manifest here because the regressed
    // arm itself is decided in `run` before any probe.
    let Some(manifest) = resolve_manifest(paths, now).into_manifest() else {
        let determination =
            RungDetermination::r2(now, R2Reason::ManifestUnavailable, None, &provenance);
        write_rung_record_silently(paths, &determination.record);
        return determination;
    };
    let Some(agent_binding) = resolved_agent_binding(&manifest, cwd) else {
        let determination = RungDetermination::r2(
            now,
            R2Reason::AgentBindingUnavailable,
            Some(manifest.manifest_version),
            &provenance,
        );
        write_rung_record_silently(paths, &determination.record);
        return determination;
    };

    let discovery = probe_governance(
        paths,
        &connection_file,
        cwd,
        deadline,
        &agent_binding.agent_id,
    );
    let determination = match discovery {
        ProbeResult::Ready { module_id } => {
            match find_ambient_agent_credential(&manifest.detectors) {
                Some(source) => {
                    let mut determination = RungDetermination::r2(
                        now,
                        R2Reason::AgentCredentialsPresent,
                        Some(manifest.manifest_version),
                        &provenance,
                    );
                    determination
                        .record
                        .inputs
                        .insert("agent_credentials_present".to_string(), source);
                    determination
                        .record
                        .inputs
                        .insert("catalog_holder".to_string(), module_id);
                    determination
                }
                None => RungDetermination::r3(now, manifest.manifest_version, &provenance),
            }
        }
        ProbeResult::Unreachable => {
            RungDetermination::r2(now, R2Reason::DaemonUnreachable, None, &provenance)
        }
        ProbeResult::NoRoute => {
            RungDetermination::r2(now, R2Reason::CatalogGhRouteAbsent, None, &provenance)
        }
        // Keep the holder-unbound status diagnostic distinct from an absent
        // repository binding. Dispatch no longer consumes either reason.
        ProbeResult::Unbound => {
            RungDetermination::r2(now, R2Reason::GhRouteHolderUnbound, None, &provenance)
        }
        ProbeResult::TimedOut => cached
            .filter(|record| record.fresh_at(now))
            .map(RungDetermination::cached)
            .unwrap_or_else(|| RungDetermination::r1(now, R1Reason::DiscoveryBudgetExhausted)),
    };

    if determination.record.rung != Rung::R1 {
        write_rung_record_silently(paths, &determination.record);
    }
    determination
}

fn configured_connection_file() -> Option<PathBuf> {
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    configured_connection_file_from(xdg_config_home.as_deref(), home.as_deref())
}

fn configured_connection_file_from(
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<PathBuf> {
    // The shim uses the same user-tier resolver as subc: `$XDG_CONFIG_HOME/cortexkit/aft.jsonc`,
    // then `~/.config/cortexkit/aft.jsonc`. XDG selects only the trusted user's
    // config location; it cannot select a project file or alter the configured
    // connection. An invalid path resolves to `None`; the caller decides whether
    // that means structural passthrough or an unavailable governed route.
    let config_path = crate::subc_config::user_config_path_from(xdg_config_home, home)?;
    let doc = fs::read_to_string(config_path).ok()?;
    connection_file_from_config_doc(&doc).filter(|path| path.is_file())
}

/// Read the raw user-tier `aft.jsonc` document for the shim's config gates.
/// `None` means the config file was absent or unreadable, which the rung
/// determination treats as "no user config" (structural rungs decide).
fn read_user_config_doc() -> Option<String> {
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let config_path =
        crate::subc_config::user_config_path_from(xdg_config_home.as_deref(), home.as_deref())?;
    fs::read_to_string(config_path).ok()
}

/// Read the `gh_shim.enabled` operator gate from the user config document.
/// `None` means the key is absent or the document is unparseable, in which case
/// the shim stays enabled (default true). Only an explicit `false` disables.
fn gh_shim_enabled_from_config_doc(doc: &str) -> Option<bool> {
    let value: Value = serde_json::from_str(&crate::jsonc::strip_jsonc(doc)).ok()?;
    value.get("gh_shim")?.get("enabled")?.as_bool()
}

fn connection_file_from_config_doc(doc: &str) -> Option<PathBuf> {
    let value: Value = serde_json::from_str(&crate::jsonc::strip_jsonc(doc)).ok()?;
    let raw = value.get("subc")?.get("connection_file")?.as_str()?.trim();
    let path = PathBuf::from(raw);
    (!raw.is_empty() && path.is_absolute()).then_some(path)
}

fn load_rung_record(paths: &StatePaths) -> Option<RungRecord> {
    serde_json::from_slice(&fs::read(&paths.rung).ok()?).ok()
}

fn write_rung_record_silently(paths: &StatePaths, record: &RungRecord) {
    let Ok(bytes) = serde_json::to_vec(record) else {
        return;
    };
    let _ = fs::create_dir_all(&paths.root);
    let temporary = paths.root.join("rung-cache.json.tmp");
    if fs::write(&temporary, bytes).is_ok() {
        let _ = fs::rename(temporary, &paths.rung);
    }
}

#[derive(Debug)]
enum ProbeResult {
    Ready { module_id: String },
    Unreachable,
    NoRoute,
    Unbound,
    TimedOut,
}

fn probe_governance(
    paths: &StatePaths,
    connection_file: &Path,
    cwd: &Path,
    deadline: std::time::Instant,
    agent_id: &str,
) -> ProbeResult {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return ProbeResult::TimedOut;
    }
    let connection_file = connection_file.to_path_buf();
    let project_root = project_root_for(cwd);
    let record_paths = paths.clone();
    let agent_id = agent_id.to_string();
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    else {
        return ProbeResult::Unreachable;
    };

    // `tokio::time::timeout` creates its timer immediately. Building that
    // future as a `block_on` argument happens before the runtime enters its
    // context, so the timer's reactor lookup panics in this synchronous CLI.
    // Construct it from inside the entered future instead.
    match runtime.block_on(async move {
        tokio::time::timeout(remaining, async move {
            let options = ConsumerOptions {
                call_timeout: remaining,
                ..ConsumerOptions::default()
            };
            let consumer = SubcConsumer::connect(&connection_file, options)
                .await
                .map_err(|_| ProbeResult::Unreachable)?;
            let catalog = consumer
                .catalog_list()
                .await
                .map_err(|_| ProbeResult::Unreachable)?;
            let holder = route_holder(&catalog.modules);
            record_unexpected_gh_route_advertisers(&record_paths, &holder.unexpected_advertisers);
            let Some(module_id) = holder.module_id else {
                return Err(ProbeResult::NoRoute);
            };
            let identity = BindIdentity {
                project_root: project_root.to_string_lossy().into_owned().into(),
                harness: "aft-gh-shim".to_string(),
                session: gh_session_id(&agent_id),
            };
            let route = consumer
                .open_route(
                    RouteTarget::ManagementSurface {
                        module_id: module_id.clone(),
                    },
                    identity,
                    CallOptions::default(),
                )
                .await
                .map_err(|_| ProbeResult::Unbound)?;
            let _ = consumer
                .close_handle(&route, CloseRouteOptions::default())
                .await;
            Ok(module_id)
        })
        .await
    }) {
        Ok(Ok(module_id)) => ProbeResult::Ready { module_id },
        Ok(Err(result)) => result,
        Err(_) => ProbeResult::TimedOut,
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
struct RouteHolder {
    module_id: Option<String>,
    unexpected_advertisers: Vec<String>,
}

fn route_holder(entries: &[subc_client_rs::CatalogEntry]) -> RouteHolder {
    select_route_holder(entries.iter().filter_map(|entry| {
        entry
            .roles
            .iter()
            .any(|role| {
                matches!(
                    role,
                    ProviderRole::ManagementSurface { operations, .. }
                        if operations.iter().any(|operation| operation.name == ROUTING_OPERATION)
                )
            })
            .then(|| entry.module_id.clone())
    }))
}

fn select_route_holder(advertisers: impl IntoIterator<Item = String>) -> RouteHolder {
    let mut holder = None;
    let mut unexpected_advertisers = BTreeSet::new();
    for advertiser in advertisers {
        // Governed routes carry identity-bearing writes, so only prefrontal-core may
        // hold `gh.route`; another module advertising it must not capture the route.
        // The holder module identifies the routing server, not the bound agent. Using
        // its module ID would merge all agents into one audit and rate-accounting session.
        if advertiser == ROUTING_HOLDER_MODULE_ID {
            holder.get_or_insert(advertiser);
        } else {
            unexpected_advertisers.insert(advertiser);
        }
    }
    RouteHolder {
        module_id: holder,
        unexpected_advertisers: unexpected_advertisers.into_iter().collect(),
    }
}

fn project_root_for(cwd: &Path) -> PathBuf {
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    canonical
        .ancestors()
        .find(|path| path.join(".git").exists())
        .map(Path::to_path_buf)
        .unwrap_or(canonical)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct AgentBinding {
    repo: String,
    agent_id: String,
}

fn resolved_agent_binding(manifest: &Manifest, cwd: &Path) -> Option<AgentBinding> {
    let project_root = project_root_for(cwd);
    let repo = repository_key_from_origin(&project_root)?;
    manifest
        .bindings
        .get(&repo)
        .cloned()
        .map(|agent_id| AgentBinding { repo, agent_id })
}

fn co_author_line(paths: &StatePaths) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let manifest = load_manifest(paths, unix_seconds()).ok()?;
    let binding = resolved_agent_binding(&manifest, &cwd)?;
    let login = binding.agent_id;
    if !valid_github_login(&login) {
        return None;
    }
    let numeric_id =
        cached_numeric_id(paths, &login).or_else(|| resolve_and_cache_numeric_id(paths, &login))?;
    Some(format!(
        "Co-authored-by: {login} <{numeric_id}+{login}@users.noreply.github.com>"
    ))
}

fn valid_github_login(login: &str) -> bool {
    let core = login.strip_suffix("[bot]").unwrap_or(login);
    !core.is_empty()
        && core.len() <= 100
        && !core.starts_with('-')
        && !core.ends_with('-')
        && core
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn cached_numeric_ids(paths: &StatePaths) -> BTreeMap<String, u64> {
    fs::read(&paths.numeric_ids)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn cached_numeric_id(paths: &StatePaths, login: &str) -> Option<u64> {
    cached_numeric_ids(paths)
        .get(login)
        .copied()
        .filter(|id| *id > 0)
}

fn resolve_and_cache_numeric_id(paths: &StatePaths, login: &str) -> Option<u64> {
    let image = executing_image();
    let real_gh = resolve_real_gh(&image)?;
    let encoded_login = url::form_urlencoded::byte_serialize(login.as_bytes()).collect::<String>();
    let output = Command::new(real_gh)
        .args(["api", &format!("users/{encoded_login}"), "--jq", ".id"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let numeric_id = String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)?;
    let mut ids = cached_numeric_ids(paths);
    ids.insert(login.to_string(), numeric_id);
    write_numeric_ids_silently(paths, &ids);
    Some(numeric_id)
}

fn write_numeric_ids_silently(paths: &StatePaths, ids: &BTreeMap<String, u64>) {
    let Ok(bytes) = serde_json::to_vec(ids) else {
        return;
    };
    if fs::create_dir_all(&paths.root).is_err() {
        return;
    }
    let temporary = paths.root.join("numeric-ids.json.tmp");
    if fs::write(&temporary, bytes).is_ok() {
        #[cfg(windows)]
        let _ = fs::remove_file(&paths.numeric_ids);
        let _ = fs::rename(temporary, &paths.numeric_ids);
    }
}

fn repository_key_from_origin(project_root: &Path) -> Option<String> {
    // The binding key comes from parsing the local origin remote, not a network
    // lookup, so a signed manifest selects the same agent when offline.
    let remote = origin_remote(project_root)?;
    canonical_repository_key(&remote)
}

fn origin_remote(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
        .map(|remote| remote.trim().to_string())
        .filter(|remote| !remote.is_empty())
}

fn canonical_repository_key(value: &str) -> Option<String> {
    let remote = value.trim().trim_end_matches('/');
    let path = if let Some(path) = [
        "https://github.com/",
        "http://github.com/",
        "ssh://git@github.com/",
        "git://github.com/",
        "git@github.com:",
        "github.com/",
    ]
    .iter()
    .find_map(|prefix| remote.strip_prefix(prefix))
    {
        path
    } else if remote.contains("://") || remote.contains('@') || remote.contains(':') {
        // Repository bindings identify GitHub repositories. A foreign remote is
        // intentionally unmapped rather than treated as an owner/name string.
        return None;
    } else {
        remote
    }
    .trim_end_matches(".git")
    .trim_matches('/');
    let mut parts = path.split('/');
    let owner = parts.next()?.trim();
    let repository = parts.next()?.trim();
    (!owner.is_empty() && !repository.is_empty() && parts.next().is_none()).then(|| {
        format!(
            "{}/{}",
            owner.to_ascii_lowercase(),
            repository.to_ascii_lowercase()
        )
    })
}

fn gh_session_id(agent_id: &str) -> String {
    format!("gh-shim:{agent_id}")
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Detectors {
    #[serde(default)]
    wrapper_config_dirs: Vec<String>,
    #[serde(default)]
    credential_env_names: Vec<String>,
}

fn find_ambient_agent_credential(detectors: &Detectors) -> Option<String> {
    for name in &detectors.credential_env_names {
        if std::env::var_os(name).is_some() {
            return Some(format!("env:{name}"));
        }
    }

    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    for raw_pattern in &detectors.wrapper_config_dirs {
        let pattern = expand_home_pattern(raw_pattern, home.as_deref());
        let paths = if pattern.contains(['*', '?', '[', '{']) {
            // Never let an ambient home-directory glob cross a mount: a vanished
            // child ReadDir can panic in Drop after closedir reports ENXIO.
            crate::walk_boundary::expand_glob_same_file_system(&pattern).unwrap_or_default()
        } else {
            vec![PathBuf::from(pattern)]
        };
        for path in paths {
            if path.is_dir() {
                return Some(format!("path:{}", path.display()));
            }
        }
    }

    // `GH_CONFIG_DIR` is only inspected as a metadata path. The basename is
    // compared to the manifest's declared wrapper-dir glob, so the operator's
    // normal gh configuration remains outside this detector inventory.
    let configured = std::env::var_os("GH_CONFIG_DIR").map(PathBuf::from)?;
    if !configured.is_dir() {
        return None;
    }
    let name = configured.file_name()?.to_string_lossy();
    detectors
        .wrapper_config_dirs
        .iter()
        .any(|pattern| {
            Path::new(pattern).file_name().is_some_and(|glob_name| {
                glob::Pattern::new(&glob_name.to_string_lossy()).is_ok_and(|p| p.matches(&name))
            })
        })
        .then(|| format!("path:{}", configured.display()))
}

fn expand_home_pattern(pattern: &str, home: Option<&Path>) -> String {
    pattern
        .strip_prefix("~/")
        .and_then(|suffix| home.map(|home| home.join(suffix).to_string_lossy().into_owned()))
        .unwrap_or_else(|| pattern.to_string())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum TupleDecl {
    Name(String),
    Details {
        tuple: String,
        #[serde(default)]
        platform: Vec<String>,
        #[serde(default)]
        api_match: Option<String>,
        // Signed manifests key this prose as `reasoning` (v10 rows). Without the
        // alias the parse silently DROPPED all signed justification text - the
        // signature verifies the raw bytes first, then serde discarded the
        // unknown field, so every cache-derived view showed rationale: null
        // while the signed artifact carried the prose (found by CKCRED's
        // structural diff during the v11 ceremony).
        #[serde(default, alias = "reasoning")]
        rationale: Option<String>,
    },
}

impl TupleDecl {
    fn tuple(&self) -> &str {
        match self {
            Self::Name(name) => name,
            Self::Details { tuple, .. } => tuple,
        }
    }

    fn platform(&self) -> &[String] {
        match self {
            Self::Name(_) => &[],
            Self::Details { platform, .. } => platform,
        }
    }

    fn empty_api_match_has_rationale(&self) -> bool {
        match self {
            Self::Details {
                api_match: Some(api_match),
                rationale,
                ..
            } if api_match.is_empty() => rationale
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty()),
            _ => true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ApiRule {
    method: String,
    path_glob: String,
    tier: Tier,
    #[serde(default)]
    platform: Vec<String>,
    #[serde(default)]
    rationale: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Canonicalization {
    #[serde(default)]
    argv_forms: Vec<String>,
    #[serde(default)]
    target_fields: Vec<String>,
    #[serde(default)]
    body_fields: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct RepositorySection {
    #[serde(default)]
    tiers: BTreeMap<Tier, Vec<TupleDecl>>,
    #[serde(default, alias = "remove")]
    removed_tuples: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Manifest {
    artifact_id: String,
    manifest_version: u64,
    schema_floor: u64,
    /// When the signer issued this manifest. This signed provenance metadata is
    /// displayed in status but does not expire a human-approved sign-once
    /// artifact; only an implausibly future issue time is malformed.
    issued_at_unix_secs: u64,
    #[serde(default)]
    detectors: Detectors,
    #[serde(default)]
    tiers: BTreeMap<Tier, Vec<TupleDecl>>,
    #[serde(default)]
    api_rules: Vec<ApiRule>,
    #[serde(default)]
    canonicalization: BTreeMap<String, Canonicalization>,
    #[serde(default)]
    repository_sections: BTreeMap<String, RepositorySection>,
    #[serde(default)]
    bindings: BTreeMap<String, String>,
}

impl Manifest {
    fn validate(&self) -> Result<(), String> {
        if self.artifact_id != MANIFEST_ARTIFACT_ID {
            return Err(format!("unexpected artifact id {}", self.artifact_id));
        }
        if self.manifest_version == 0 {
            return Err("manifest_version must be positive".to_string());
        }

        let mut declared = BTreeMap::<String, Tier>::new();
        for (tier, entries) in &self.tiers {
            for entry in entries {
                let tuple = normalized_tuple(entry.tuple())?;
                if entry.platform().is_empty() {
                    return Err(format!("tuple {tuple} is missing its platform declaration"));
                }
                if !entry.empty_api_match_has_rationale() {
                    return Err(format!(
                        "tuple {tuple} has an empty api_match without rationale"
                    ));
                }
                if let Some(previous) = declared.insert(tuple.clone(), *tier) {
                    return Err(format!(
                        "tuple {tuple} is declared in both {previous:?} and {tier:?}"
                    ));
                }
            }
        }

        let mut api_declared = BTreeSet::new();
        for rule in &self.api_rules {
            if rule.method.trim().is_empty() || rule.path_glob.trim().is_empty() {
                if rule.path_glob.is_empty()
                    && rule
                        .rationale
                        .as_deref()
                        .is_some_and(|text| !text.trim().is_empty())
                {
                    continue;
                }
                return Err("api rule requires method and non-empty path_glob".to_string());
            }
            if rule.platform.is_empty() {
                return Err(format!(
                    "api rule {} {} is missing its platform declaration",
                    rule.method, rule.path_glob
                ));
            }
            let key = format!("{} {}", rule.method.to_ascii_uppercase(), rule.path_glob);
            if !api_declared.insert(key.clone()) {
                return Err(format!("api rule {key} is declared more than once"));
            }
        }

        let governed = self.tiers.get(&Tier::Governed).cloned().unwrap_or_default();
        for entry in &governed {
            let tuple = normalized_tuple(entry.tuple())?;
            let Some(canonical) = self.canonicalization.get(&tuple) else {
                return Err(format!("governed tuple {tuple} lacks canonicalization"));
            };
            if canonical.argv_forms.is_empty() || canonical.target_fields.is_empty() {
                return Err(format!(
                    "governed tuple {tuple} has incomplete canonicalization"
                ));
            }
        }
        for tuple in self.canonicalization.keys() {
            if declared.get(tuple) != Some(&Tier::Governed) {
                return Err(format!(
                    "canonicalization {tuple} does not name a governed tuple"
                ));
            }
        }

        for (repository, agent_id) in &self.bindings {
            if canonical_repository_key(repository).as_deref() != Some(repository.as_str()) {
                return Err(format!(
                    "binding repository {repository} is not canonical owner/name"
                ));
            }
            if agent_id.trim().is_empty() || agent_id.trim() != agent_id {
                return Err(format!(
                    "binding repository {repository} has an invalid agent id"
                ));
            }
        }

        for (repository, section) in &self.repository_sections {
            for removed in &section.removed_tuples {
                if !declared.contains_key(&normalized_tuple(removed)?) {
                    return Err(format!(
                        "repository section {repository} removes undeclared tuple {removed}"
                    ));
                }
            }
            for (tier, entries) in &section.tiers {
                for entry in entries {
                    let tuple = normalized_tuple(entry.tuple())?;
                    let Some(base) = declared.get(&tuple) else {
                        return Err(format!(
                            "repository section {repository} adds tuple {tuple}"
                        ));
                    };
                    if tier.rank() < base.rank() {
                        return Err(format!(
                            "repository section {repository} lowers tuple {tuple}"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn tier_for_tuple(&self, tuple: &str, platform: &str) -> Option<Tier> {
        self.tiers.iter().find_map(|(tier, entries)| {
            entries
                .iter()
                .any(|entry| {
                    normalized_tuple(entry.tuple()).ok().as_deref() == Some(tuple)
                        && platform_matches(entry.platform(), platform)
                })
                .then_some(*tier)
        })
    }
}

fn normalized_tuple(value: &str) -> Result<String, String> {
    let words = value
        .split_whitespace()
        .map(|word| word.to_ascii_lowercase())
        .collect::<Vec<_>>();
    (!words.is_empty())
        .then(|| words.join(" "))
        .ok_or_else(|| "tuple cannot be empty".to_string())
}

fn platform_matches(platforms: &[String], current: &str) -> bool {
    platforms
        .iter()
        .any(|platform| platform.eq_ignore_ascii_case(current))
}

/// Envelope v2: the manifest body travels as the EXACT bytes the signer
/// published. `manifest_bytes` is an opaque string holding that file's
/// contents verbatim; the verifier checks the signature over those bytes
/// BEFORE parsing them, so the signature contract is "the signer signed the
/// file it publishes" and no canonicalization rule exists on this side.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct SignedManifest {
    artifact_id: String,
    envelope_version: u64,
    key_id: String,
    /// Advisory local metadata only: when this machine stored the artifact.
    /// It is not a validity input and re-stamping it cannot alter the signed
    /// manifest provenance.
    fetched_at_unix_secs: u64,
    signature: String,
    manifest_bytes: String,
}

#[derive(Clone, Debug)]
struct VerifiedManifest {
    manifest: Manifest,
    verified_by_key_id: String,
}

#[derive(Clone, Debug)]
enum ManifestProblem {
    Missing,
    Invalid(String),
    BelowFloor {
        manifest_floor: u64,
    },
    /// The manifest is validly signed but its version is below the newest
    /// version ever accepted on this machine: a rollback incident, never an
    /// ordinary out-of-order arrival.
    RolledBack {
        manifest_version: u64,
        newest_accepted: u64,
    },
}

impl ManifestProblem {
    fn diagnostic(&self) -> SelfReportDiagnostic {
        match self {
            Self::Missing => SelfReportDiagnostic::ManifestUnavailable,
            Self::Invalid(_) => SelfReportDiagnostic::ManifestInvalid,
            Self::BelowFloor { .. } => SelfReportDiagnostic::ManifestBelowFloor,
            Self::RolledBack { .. } => SelfReportDiagnostic::ManifestRollback,
        }
    }

    fn status_label(&self) -> String {
        match self {
            Self::Missing => "unavailable".to_string(),
            Self::Invalid(error) => format!("invalid ({error})"),
            Self::BelowFloor { manifest_floor } => format!(
                "{} (manifest floor {manifest_floor}, shim floor {SCHEMA_FLOOR})",
                RefusalCode::ManifestBelowFloor.as_str()
            ),
            Self::RolledBack {
                manifest_version,
                newest_accepted,
            } => format!(
                "{} (manifest version {manifest_version}, newest accepted version {newest_accepted})",
                SelfReportDiagnostic::ManifestRollback.as_str()
            ),
        }
    }

    fn fallback_notice_reason(&self) -> String {
        match self {
            Self::Missing => "manifest unavailable".to_string(),
            Self::Invalid(reason) => reason.clone(),
            Self::BelowFloor { manifest_floor } => {
                format!("manifest floor {manifest_floor}, shim floor {SCHEMA_FLOOR}")
            }
            Self::RolledBack {
                manifest_version,
                newest_accepted,
            } => {
                format!("manifest version {manifest_version}, newest accepted version {newest_accepted}")
            }
        }
    }

    fn untrusted_manifest_key_steering(&self) -> Option<&'static str> {
        match self {
            Self::Invalid(reason) if reason.starts_with("untrusted manifest key id ") => {
                Some(UNTRUSTED_MANIFEST_KEY_STEERING)
            }
            Self::Missing
            | Self::Invalid(_)
            | Self::BelowFloor { .. }
            | Self::RolledBack { .. } => None,
        }
    }
}

/// Verifier-site contract for the signed routing manifest.
///
/// RAW DISTRIBUTED BYTES. The manifest body travels inside the envelope as the
/// exact bytes the signer published (`manifest_bytes`). This function verifies
/// the received bytes FIRST and parses them into a `Manifest` SECOND. The
/// signer signs the file it publishes; no canonicalization rule exists on this
/// side, so no field reorder, re-indent, or re-encode can break (or silently
/// reshape) the signature contract across languages. Verifying a parsed and
/// re-serialized struct instead would make every serializer a party to the
/// signature.
///
/// VERSION-MONOTONIC VALIDITY. Manifest approval is a human ceremony performed
/// once per signature, not a periodic lease. Expiring a sign-once artifact
/// converts approval cadence into a scheduled outage. A verified manifest stays
/// valid regardless of age; the local version high-water mark refuses a
/// validly-signed version below the newest accepted version, which is the honest
/// replay defense. `issued_at_unix_secs` remains signed provenance metadata and
/// rejects only an implausibly future timestamp.
///
/// TWO-SIDED BOUND (the custody bar stays at config integrity). The governed
/// EXECUTION vocabulary is compiled into the route holder (vendored
/// classification); no manifest can widen what the holder executes. The
/// manifest governs shim-side routing selection only. Manifest tampering is
/// therefore bounded above by the holder's vendored set and below by the
/// shim's refusal arms. If classification ever moves INTO the manifest, the
/// trust root flips from integrity to authority and the custody design must be
/// revisited first.
///
/// Delta property, phrased for the manifest approver: NARROWING a manifest
/// WIDENS the key-compromise surface. The delta is the compiled vocabulary
/// minus what this manifest routes; every operation a manifest stops routing
/// joins the set a compromised signing key could re-enable. The approval
/// question for a narrowing change is "am I content that a key compromise
/// re-enables exactly the operations this manifest stops routing", not "does
/// this look tighter". The holder-side vendored vocabulary guards the widening
/// direction; this line guards the narrowing one.
///
/// DORMANCY VALVE AND LOCAL STATE. With no manifest artifact on disk the shim
/// is dormant and passes invocations through (R2, reason
/// `manifest_unavailable`). That valve's weakness — a local downgrade to
/// dormant by deleting the artifact — is only reachable by an adversary with
/// local write access, who can equally patch the compiled-in trust set or this
/// verifier itself; the weakness is only reachable by an adversary the design
/// already cannot survive. The same argument covers the local state this
/// verifier maintains: the last-valid manifest cache and the monotonic version
/// high-water mark are enforcement conveniences, not a security boundary, and
/// an adversary who can delete, forge, or lower them can patch the verifier.
///
/// TOKEN LANGUAGE. The holder executes governed calls under full-installation
/// GitHub App tokens held in custody; operation gating is holder-side
/// classification over the routed request. The shim never holds any token in
/// either direction.
fn load_manifest(paths: &StatePaths, now: u64) -> Result<Manifest, ManifestProblem> {
    load_manifest_with_trust_set(paths, now, compiled_manifest_trust_set())
        .map(|verified| verified.manifest)
}

fn load_manifest_with_trust_set(
    paths: &StatePaths,
    now: u64,
    trust_set: &[Option<ManifestTrustKey>],
) -> Result<VerifiedManifest, ManifestProblem> {
    let bytes = fs::read(&paths.manifest).map_err(|_| ManifestProblem::Missing)?;
    let envelope: SignedManifest = serde_json::from_slice(&bytes)
        .map_err(|error| ManifestProblem::Invalid(error.to_string()))?;
    if envelope.artifact_id != MANIFEST_ARTIFACT_ID {
        return Err(ManifestProblem::Invalid("artifact id mismatch".to_string()));
    }
    if envelope.envelope_version != ENVELOPE_VERSION {
        return Err(ManifestProblem::Invalid(format!(
            "unsupported envelope version {} (this shim verifies envelope version {ENVELOPE_VERSION})",
            envelope.envelope_version
        )));
    }
    // Verify the received bytes FIRST, parse SECOND (contract above).
    let VerifiedManifest {
        manifest,
        verified_by_key_id,
    } = verify_manifest_signature_with_provenance(&envelope, trust_set)?;
    manifest.validate().map_err(ManifestProblem::Invalid)?;
    if manifest.schema_floor < SCHEMA_FLOOR {
        return Err(ManifestProblem::BelowFloor {
            manifest_floor: manifest.schema_floor,
        });
    }
    // Monotonic version high-water mark: a manifest older than the newest ever
    // accepted here is refused as a rollback incident. Version, not artifact age,
    // prevents replay of a past manifest that may carry a wider vocabulary.
    let newest_accepted = version_high_water(paths);
    if manifest.manifest_version < newest_accepted {
        return Err(ManifestProblem::RolledBack {
            manifest_version: manifest.manifest_version,
            newest_accepted,
        });
    }
    if manifest.issued_at_unix_secs > now + ISSUED_AT_FUTURE_SKEW.as_secs() {
        return Err(ManifestProblem::Invalid(format!(
            "issued_at_unix_secs {} is more than {} seconds in the future",
            manifest.issued_at_unix_secs,
            ISSUED_AT_FUTURE_SKEW.as_secs()
        )));
    }
    // Accepted: advance the high-water mark and refresh the last-valid cache
    // that the regressed-manifest arm classifies from. Both are local state
    // under the dormancy-valve argument documented above.
    if manifest.manifest_version > newest_accepted {
        write_version_high_water(paths, manifest.manifest_version);
    }
    write_last_valid_manifest(paths, &manifest);
    Ok(VerifiedManifest {
        manifest,
        verified_by_key_id,
    })
}

/// Verify the signature over the envelope's exact manifest bytes, then parse.
/// No manifest content is interpreted before its bytes verify.
#[cfg(test)]
fn verify_manifest_signature(envelope: &SignedManifest) -> Result<Manifest, ManifestProblem> {
    verify_manifest_signature_with(envelope, compiled_manifest_trust_set())
}

#[cfg(test)]
fn verify_manifest_signature_with(
    envelope: &SignedManifest,
    trust_set: &[Option<ManifestTrustKey>],
) -> Result<Manifest, ManifestProblem> {
    verify_manifest_signature_with_provenance(envelope, trust_set).map(|verified| verified.manifest)
}

fn verify_manifest_signature_with_provenance(
    envelope: &SignedManifest,
    trust_set: &[Option<ManifestTrustKey>],
) -> Result<VerifiedManifest, ManifestProblem> {
    let Some(key) = trust_set
        .iter()
        .flatten()
        .find(|slot| slot.key_id == envelope.key_id)
        .copied()
    else {
        return Err(ManifestProblem::Invalid(format!(
            "untrusted manifest key id {}",
            envelope.key_id
        )));
    };
    let signature = base64::engine::general_purpose::STANDARD
        .decode(&envelope.signature)
        .map_err(|_| ManifestProblem::Invalid("invalid detached signature encoding".to_string()))?;
    UnparsedPublicKey::new(&ED25519, key.public_key)
        .verify(envelope.manifest_bytes.as_bytes(), &signature)
        .map_err(|_| {
            ManifestProblem::Invalid("detached signature verification failed".to_string())
        })?;
    let manifest = serde_json::from_str(&envelope.manifest_bytes).map_err(|error| {
        ManifestProblem::Invalid(format!("signed manifest bytes failed to parse: {error}"))
    })?;
    Ok(VerifiedManifest {
        manifest,
        verified_by_key_id: key.key_id.to_string(),
    })
}

/// One trusted manifest signing key: a stable key id plus the Ed25519 public
/// key bytes that id binds.
#[derive(Clone, Copy)]
struct ManifestTrustKey {
    key_id: &'static str,
    public_key: &'static [u8],
}

// A manifest signature is the barrier preventing an agent from editing its own
// cache to turn a governed verb into a mechanical one. The development key is
// deliberately compiled only in debug builds so fixtures can exercise R3. A
// release build has no trust root until the separately reviewed CKCRED custody
// release supplies one, which keeps release binaries at R2 rather than making a
// governance claim with a test key.
//
// TWO-KEY TRUST SET. The release trust array ships with TWO key slots from day
// one: the live signing key and a cold standby with a distinct key id. The dev
// set keeps its single test key.
//
// The production signing keys are minted and held by the key-custody process
// outside this repository; a separately reviewed release copies each approved
// public key into these slots (the private half never approaches the build).
// Until that happens both slots stay empty and release binaries remain at R2.
//
// Two-release rotation procedure once the slots are filled:
//   1. Ship a release whose standby slot carries the standby key. Both slots
//      verify; the live key still signs. The standby key comes from custody,
//      never from a self-generated filler.
//   2. Promote the standby by shipping a manifest signed by it. The trust set
//      already accepts it, so promotion does not depend on updating binaries
//      first.
//   3. One release later, remove the old key. Between promotion and removal a
//      compromise of the old key can still sign, so the removal release is
//      part of the rotation rather than optional cleanup.
//
// Slot layout: index 0 is the LIVE signing key, index 1 is the COLD STANDBY.
// The first manifest signature verifying under a newly installed live key is
// the stored-key-equals-published-half acceptance test for that installation.
//
// LIVE KEY PROVENANCE (2026-08-27 CKCRED ceremony): `signing:gh-manifest-root:1`,
// minted in-vault via `ck auth mint-signing-key` (private half never exported;
// extraction-refusal proven by a live `credential.get` attack against a bearer
// handle, then the handle revoked). Public half read back over the route plane
// and independently verified in Node's stdlib Ed25519 with a tamper control.
const PROD_MANIFEST_KEY_ID: &str = "c9ad111282d1da10";
const PROD_MANIFEST_PUBLIC_KEY: [u8; 32] = [
    0x5f, 0x4c, 0x81, 0x90, 0x18, 0xe2, 0xb6, 0x8d, 0x18, 0xdb, 0xce, 0x6a, 0xc3, 0x6f, 0x9b, 0x84,
    0x65, 0x28, 0x84, 0x14, 0x75, 0x55, 0xe8, 0x44, 0x2e, 0xf7, 0x6d, 0x7f, 0xb4, 0x7a, 0x42, 0xf4,
];
const PROD_MANIFEST_TRUST_KEY: ManifestTrustKey = ManifestTrustKey {
    key_id: PROD_MANIFEST_KEY_ID,
    public_key: &PROD_MANIFEST_PUBLIC_KEY,
};

#[cfg(not(debug_assertions))]
const RELEASE_MANIFEST_TRUST_SET: &[Option<ManifestTrustKey>; 2] = &[
    Some(PROD_MANIFEST_TRUST_KEY), // live
    None,                          // cold standby (filled by a future custody release)
];

#[cfg(debug_assertions)]
const DEV_MANIFEST_KEY_ID: &str = "gh-routing-dev-test-key-v1";
#[cfg(debug_assertions)]
const DEV_MANIFEST_PUBLIC_KEY: [u8; 32] = [
    0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07, 0x3a,
    0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07, 0x51, 0x1a,
];
// Debug images verify BOTH eras: the production root (so fleet manifests work
// on dev builds) and the dev test key (so fixtures can exercise R3 without a
// custody round-trip). The envelope's key_id selects at verify time.
#[cfg(debug_assertions)]
const DEV_MANIFEST_TRUST_SET: &[Option<ManifestTrustKey>; 2] = &[
    Some(PROD_MANIFEST_TRUST_KEY),
    Some(ManifestTrustKey {
        key_id: DEV_MANIFEST_KEY_ID,
        public_key: &DEV_MANIFEST_PUBLIC_KEY,
    }),
];

fn compiled_manifest_trust_set() -> &'static [Option<ManifestTrustKey>] {
    #[cfg(debug_assertions)]
    {
        DEV_MANIFEST_TRUST_SET
    }
    #[cfg(not(debug_assertions))]
    {
        RELEASE_MANIFEST_TRUST_SET
    }
}

fn trust_set_key_ids(trust_set: &[Option<ManifestTrustKey>]) -> Vec<&'static str> {
    trust_set.iter().flatten().map(|key| key.key_id).collect()
}

/// Outcome of resolving the installed manifest artifact for an invocation.
///
/// The state is keyed on what is INSTALLED on disk, never on memory of past
/// validation: every call re-reads the artifact and re-derives its
/// disposition from the artifact plus the local last-valid cache.
#[derive(Debug)]
enum ManifestResolution {
    /// The installed artifact verified: normal classification.
    Active(Manifest),
    /// The installed artifact failed validation after a prior valid manifest:
    /// governed/admin tuples the cache classifies are refused, while mechanical
    /// operations pass through.
    Regressed {
        manifest: Manifest,
        problem: ManifestProblem,
    },
    /// An artifact is installed but failed validation before this machine ever
    /// accepted one, so the invocation passes through with an identity notice.
    Invalid(ManifestProblem),
    /// No manifest artifact is present, so delegate without manifest-based routing.
    Dormant,
}

impl ManifestResolution {
    fn manifest(&self) -> Option<&Manifest> {
        match self {
            Self::Active(manifest) | Self::Regressed { manifest, .. } => Some(manifest),
            Self::Invalid(_) | Self::Dormant => None,
        }
    }

    fn into_manifest(self) -> Option<Manifest> {
        match self {
            Self::Active(manifest) | Self::Regressed { manifest, .. } => Some(manifest),
            Self::Invalid(_) | Self::Dormant => None,
        }
    }

    fn invalid_problem(&self) -> Option<&ManifestProblem> {
        match self {
            Self::Regressed { problem, .. } | Self::Invalid(problem) => Some(problem),
            Self::Active(_) | Self::Dormant => None,
        }
    }
}

fn resolve_manifest(paths: &StatePaths, now: u64) -> ManifestResolution {
    match load_manifest(paths, now) {
        Ok(manifest) => ManifestResolution::Active(manifest),
        Err(ManifestProblem::Missing) => ManifestResolution::Dormant,
        Err(problem) => match read_last_valid_manifest(paths) {
            Some(cache) => ManifestResolution::Regressed {
                manifest: cache.manifest,
                problem,
            },
            None => ManifestResolution::Invalid(problem),
        },
    }
}

fn delegate_after_invalid_manifest_notice(args: &[OsString], problem: &ManifestProblem) -> i32 {
    // Missing manifests identify public installations and must remain silent.
    // An installed but invalid manifest instead signals a misconfigured
    // governed seat, so say which ambient identity will execute the fallback.
    eprintln!(
        "gh-shim: manifest invalid ({}); executing with ambient gh credentials",
        problem.fallback_notice_reason().replace(['\n', '\r'], " ")
    );
    delegate(args)
}

/// Disposition of one invocation under the regressed-manifest arm.
///
/// Governed and admin tuples, as classified by the last-valid manifest, fail
/// closed with a stable refusal; mechanical operations pass through
/// byte-transparently. The operator bypass does not apply here: a broken
/// manifest means the classification itself is untrusted, so no bypass can
/// promote it.
fn regressed_disposition(
    args: &[OsString],
    manifest: &Manifest,
    platform: &str,
    problem: &ManifestProblem,
) -> RegressedDisposition {
    match classify(args, manifest, platform) {
        Classification::Mechanical => RegressedDisposition::Passthrough,
        Classification::Governed { tuple, .. } | Classification::Admin { tuple } => {
            let text = match problem.untrusted_manifest_key_steering() {
                Some(steering) => {
                    format!("the manifest artifact fails validation; {tuple} is refused; {steering}")
                }
                None => format!(
                    "the manifest artifact fails validation; {tuple} is refused until the manifest is repaired"
                ),
            };
            RegressedDisposition::Refuse {
                code: RefusalCode::ManifestRegressed,
                text,
            }
        }
        Classification::Unclassified => RegressedDisposition::Refuse {
            code: RefusalCode::Unclassified,
            text:
                "no manifest declaration for this invocation (manifest artifact fails validation)"
                    .to_string(),
        },
    }
}

#[derive(Debug)]
enum RegressedDisposition {
    Passthrough,
    Refuse { code: RefusalCode, text: String },
}

/// Last manifest that fully verified on this machine. Local state under the
/// dormancy-valve argument at the verifier site: it lets the regressed-manifest
/// arm keep classifying while a broken artifact is repaired, and it is not a
/// security boundary.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct LastValidManifest {
    manifest: Manifest,
}

fn read_last_valid_manifest(paths: &StatePaths) -> Option<LastValidManifest> {
    serde_json::from_slice(&fs::read(&paths.last_valid_manifest).ok()?).ok()
}

fn write_last_valid_manifest(paths: &StatePaths, manifest: &Manifest) {
    let record = LastValidManifest {
        manifest: manifest.clone(),
    };
    let Ok(bytes) = serde_json::to_vec(&record) else {
        return;
    };
    let _ = fs::create_dir_all(&paths.root);
    let temporary = paths.last_valid_manifest.with_extension("tmp");
    if fs::write(&temporary, bytes).is_ok() {
        let _ = fs::rename(temporary, &paths.last_valid_manifest);
    }
}

/// Monotonic high-water mark: the newest `manifest_version` ever accepted on
/// this machine. A manifest below it is refused as a rollback incident. Local
/// state under the same dormancy-valve argument as the last-valid cache: an
/// adversary who can lower it can patch this verifier.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct VersionHighWater {
    newest_accepted_version: u64,
}

fn version_high_water(paths: &StatePaths) -> u64 {
    fs::read(&paths.version_high_water)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<VersionHighWater>(&bytes).ok())
        .map(|record| record.newest_accepted_version)
        .unwrap_or(0)
}

fn write_version_high_water(paths: &StatePaths, newest_accepted_version: u64) {
    let Ok(bytes) = serde_json::to_vec(&VersionHighWater {
        newest_accepted_version,
    }) else {
        return;
    };
    let _ = fs::create_dir_all(&paths.root);
    let temporary = paths.version_high_water.with_extension("tmp");
    if fs::write(&temporary, bytes).is_ok() {
        let _ = fs::rename(temporary, &paths.version_high_water);
    }
}

#[derive(Debug)]
enum Classification {
    Mechanical,
    Governed {
        tuple: String,
        canonical: Canonicalization,
    },
    Admin {
        tuple: String,
    },
    Unclassified,
}

fn is_reviewed_admin_tuple(manifest_version: u64, tuple: &str) -> bool {
    V1_ADMIN_TUPLES.contains(&tuple)
        || (manifest_version >= 9 && V9_ADMIN_TUPLES.contains(&tuple))
        || (manifest_version >= 10 && V10_ADMIN_TUPLES.contains(&tuple))
}

fn is_reviewed_edit_last_tuple(manifest_version: u64, tuple: &str) -> bool {
    manifest_version >= 10 && V10_EDIT_LAST_TUPLES.contains(&tuple)
}

fn has_exact_flag(args: &[OsString], flag: &str) -> bool {
    args.iter().any(|arg| arg.to_str() == Some(flag))
}

fn classify(args: &[OsString], manifest: &Manifest, platform: &str) -> Classification {
    let Some((verb, subcommand, _)) = command_head(args) else {
        // Keep malformed argument vectors fail-closed; a valid no-subcommand
        // vector is the mechanical case described below.
        if args.iter().any(|arg| arg.to_str().is_none()) {
            return Classification::Unclassified;
        }
        // When no subcommand is provided, the real `gh` can only show top-level
        // help or version information; it cannot make GitHub requests or change
        // the active user or account. Therefore this invocation is mechanical.
        return Classification::Mechanical;
    };
    if verb == "help" {
        // `gh help <command>` only renders upstream CLI help and has no GitHub-side effects.
        return Classification::Mechanical;
    }
    if verb == "api" {
        return classify_api(args, manifest, platform);
    }
    let tuple = match subcommand {
        Some(subcommand) => format!("{verb} {subcommand}"),
        None => verb,
    };
    // `--edit-last` is the native gh operation that edits the authenticated
    // user's own last comment. Keep this exact author-scoped form limited to
    // the explicitly allowed comment tuples. An id-addressed API PATCH remains
    // unclassified because it can edit a comment selected by ID rather than the
    // authenticated user's own last comment.
    if has_exact_flag(args, "--edit-last")
        && !is_reviewed_edit_last_tuple(manifest.manifest_version, &tuple)
    {
        return Classification::Unclassified;
    }
    // Deletion and create-if-none perform different mutations from editing the
    // authenticated user's last comment, so they must not inherit the narrowly
    // scoped --edit-last allowance.
    if has_exact_flag(args, "--delete-last") || has_exact_flag(args, "--create-if-none") {
        return Classification::Unclassified;
    }
    if READ_ONLY_ACTION_TUPLES.contains(&tuple.as_str()) {
        return Classification::Mechanical;
    }
    match manifest.tier_for_tuple(&tuple, platform) {
        Some(Tier::Mechanical) => Classification::Mechanical,
        Some(Tier::Admin) if is_reviewed_admin_tuple(manifest.manifest_version, &tuple) => {
            Classification::Admin { tuple }
        }
        Some(Tier::Governed) if V1_GOVERNED_TUPLES.contains(&tuple.as_str()) => manifest
            .canonicalization
            .get(&tuple)
            .cloned()
            .map(|canonical| Classification::Governed { tuple, canonical })
            .unwrap_or(Classification::Unclassified),
        // Only tuples named by the manifest and a generation-specific classifier
        // allowlist can be governed or admin. Command names alone do not opt an
        // entry in; a new write shape needs both a manifest declaration and a
        // matching classifier allowlist entry.
        Some(Tier::Governed | Tier::Admin) | None => Classification::Unclassified,
    }
}

fn command_head(args: &[OsString]) -> Option<(String, Option<String>, usize)> {
    let mut positionals = Vec::new();
    let mut skip_next = false;
    for (index, raw) in args.iter().enumerate() {
        let value = raw.to_str()?;
        if skip_next {
            skip_next = false;
            continue;
        }
        if matches!(value, "--repo" | "-R" | "--hostname" | "--config-dir") {
            skip_next = true;
            continue;
        }
        if value.starts_with('-') {
            continue;
        }
        positionals.push((value.to_ascii_lowercase(), index));
        if positionals.len() == 2 || positionals[0].0 == "api" {
            break;
        }
    }
    let (verb, index) = positionals.first()?.clone();
    let subcommand = positionals.get(1).map(|(value, _)| value.clone());
    Some((verb, subcommand, index))
}

fn classify_api(args: &[OsString], manifest: &Manifest, platform: &str) -> Classification {
    let Some((method, path)) = api_method_and_path(args) else {
        return Classification::Unclassified;
    };
    let matches = manifest
        .api_rules
        .iter()
        .filter(|rule| {
            rule.method.eq_ignore_ascii_case(&method)
                && platform_matches(&rule.platform, platform)
                && glob::Pattern::new(&rule.path_glob).is_ok_and(|pattern| pattern.matches(&path))
        })
        .collect::<Vec<_>>();
    if matches.is_empty() && method.eq_ignore_ascii_case("GET") {
        // A field-free GET cannot write or assert an identity, so it remains a
        // mechanical read even when the manifest has no endpoint-specific rule.
        return Classification::Mechanical;
    }
    if matches.len() != 1 {
        return Classification::Unclassified;
    }
    // API writes are not normalized into governed or ADMIN equivalents until a
    // parser accepts and validates their exact argv forms. An id-addressed
    // comment PATCH can target any contributor's comment, unlike native
    // `--edit-last`, which is scoped to the caller.
    match matches[0].tier {
        Tier::Mechanical => Classification::Mechanical,
        Tier::Governed | Tier::Admin => Classification::Unclassified,
    }
}

fn api_method_and_path(args: &[OsString]) -> Option<(String, String)> {
    let mut method = "GET".to_string();
    let mut path = None;
    let mut index = 1;
    while index < args.len() {
        let value = args[index].to_str()?;
        if matches!(value, "--method" | "-X") {
            method = args.get(index + 1)?.to_str()?.to_ascii_uppercase();
            index += 2;
            continue;
        }
        if let Some(method_value) = value.strip_prefix("--method=") {
            method = method_value.to_ascii_uppercase();
            index += 1;
            continue;
        }
        if is_api_field_argument(value) {
            // Field-bearing API forms can change request semantics independently
            // of the endpoint. Until an audited form has a reviewed parser, they
            // remain unclassified rather than inheriting a read-like API rule.
            return None;
        }
        if value.starts_with('-') {
            index += 1;
            continue;
        }
        if path.is_none() {
            path = Some(value.to_string());
        }
        index += 1;
    }
    let path = path?;
    (path != "-").then_some((method, path))
}

fn is_api_field_argument(value: &str) -> bool {
    ["--input", "--raw-field", "--field"]
        .iter()
        .any(|flag| value == *flag || value.starts_with(&format!("{flag}=")))
        || value == "-F"
        || value.starts_with("-F")
        || value == "-f"
        || value.starts_with("-f")
}

#[derive(Clone, Debug)]
struct GovernedRequest {
    action: String,
    target: Map<String, Value>,
    body: Map<String, Value>,
    repository: Option<String>,
    manifest_version: u64,
    edit_last: bool,
}

/// The normalized GitHub resource changed by a structured governed request.
#[derive(Debug, Eq, PartialEq)]
struct GithubReadMutation {
    resource_kind: GithubReadResourceKind,
    normalized_repository: String,
    resource_number: i64,
}

impl GithubReadMutation {
    fn from_governed_request(request: &GovernedRequest) -> Option<Self> {
        let resource_kind = match request.action.as_str() {
            "issue comment" | "issue reaction" => GithubReadResourceKind::Issue,
            "pr comment" | "pr review" => GithubReadResourceKind::PullRequest,
            _ => return None,
        };
        let normalized_repository = canonical_repository_key(request.repository.as_deref()?)?;
        let resource_number = request.target.get("number")?.as_str()?.parse().ok()?;
        (resource_number > 0).then_some(Self {
            resource_kind,
            normalized_repository,
            resource_number,
        })
    }
}

/// Remove stale reads only after the structured mutation result is successful.
///
/// The shim runs before AFT selects standalone or subc transport, so keeping the
/// callback here gives both execution modes identical invalidation behavior.
fn invalidate_successful_github_read_mutation(
    mutation: Option<&GithubReadMutation>,
    outcome: &RouteOutcome,
) {
    invalidate_successful_github_read_mutation_at(
        &crate::bash_background::storage_dir(None),
        mutation,
        outcome,
    );
}

fn invalidate_successful_github_read_mutation_at(
    storage_root: &Path,
    mutation: Option<&GithubReadMutation>,
    outcome: &RouteOutcome,
) {
    if !matches!(outcome, RouteOutcome::Result(_)) {
        return;
    }
    let Some(mutation) = mutation else {
        return;
    };
    let Ok(conn) = crate::db::open(&storage_root.join("aft.db")) else {
        return;
    };
    // A successful mutation can change content shared by several identities, so
    // evict every identity's cache row for the exact resource.
    let _ = invalidate_github_read_cache_resource(
        &conn,
        mutation.resource_kind,
        &mutation.normalized_repository,
        mutation.resource_number,
        None,
    );
}

fn canonicalize_governed(
    args: &[OsString],
    tuple: &str,
    canonical: &Canonicalization,
    manifest_version: u64,
) -> Result<GovernedRequest, String> {
    let (_, _, head_index) =
        command_head(args).ok_or_else(|| "missing command head".to_string())?;
    let subcommand_index = if tuple.starts_with("api ") {
        head_index
    } else {
        head_index + 1
    };
    let mut positional = Vec::new();
    let mut body = Map::new();
    let mut review_event = None;
    let mut explicit_repository = None;
    let mut edit_last = false;
    let mut index = subcommand_index + 1;
    while index < args.len() {
        let value = args[index]
            .to_str()
            .ok_or_else(|| "non-UTF-8 governed arguments are undeclared".to_string())?;
        if tuple == "pr review" {
            if let Some(event) = declared_review_event(value) {
                if review_event.replace(event.to_string()).is_some() {
                    return Err(
                        "pr review accepts only one of --approve, --comment, or --request-changes"
                            .to_string(),
                    );
                }
                index += 1;
                continue;
            }
        }
        if value == "--edit-last" {
            if !is_reviewed_edit_last_tuple(manifest_version, tuple) {
                return Err("undeclared flag --edit-last".to_string());
            }
            if edit_last {
                return Err("--edit-last may be provided only once".to_string());
            }
            edit_last = true;
        } else if value == "--repo" || value == "-R" {
            index += 1;
            let repository = args
                .get(index)
                .and_then(|arg| arg.to_str())
                .ok_or_else(|| "--repo requires a value".to_string())?;
            explicit_repository = Some(repository.to_string());
        } else if let Some(repository) = value.strip_prefix("--repo=") {
            explicit_repository = Some(repository.to_string());
        } else if let Some((field, supplied)) =
            declared_body_value(value, canonical, args.get(index + 1))?
        {
            body.insert(field, Value::String(supplied));
            if !value.contains('=') && !value.starts_with('-') {
                // Kept for completeness; declared_body_value only returns flags.
                positional.push(value.to_string());
            }
            if !value.contains('=') {
                index += 1;
            }
        } else if value.starts_with('-') {
            return Err(format!("undeclared flag {value}"));
        } else {
            positional.push(value.to_string());
        }
        index += 1;
    }

    if positional.len() != canonical.target_fields.len() {
        return Err("target positional form is undeclared".to_string());
    }
    if canonical
        .body_fields
        .iter()
        .any(|field| !body.contains_key(field))
    {
        // An explicit approve/request-changes review is valid without prose;
        // comments still need a body because upstream gh would otherwise open
        // an interactive prompt that the governed seam cannot reproduce.
        let body_optional_for_review = tuple == "pr review"
            && review_event
                .as_deref()
                .is_some_and(|event| event != "COMMENT")
            && canonical.body_fields.iter().all(|field| field == "body");
        if !body_optional_for_review {
            return Err("required declared body field is absent".to_string());
        }
    }
    if let Some(event) = review_event {
        body.insert("event".to_string(), Value::String(event));
    }
    let target = canonical
        .target_fields
        .iter()
        .cloned()
        .zip(positional)
        .map(|(field, value)| (field, Value::String(value)))
        .collect::<Map<_, _>>();
    // A global `--repo` may precede the command head, so inspect the original
    // argv before falling back to a command-local flag or remote inference.
    let repository = explicit_repo(args)
        .or(explicit_repository)
        .or_else(infer_repository_from_git)
        .map(|repository| {
            canonical_repository_key(&repository)
                .ok_or_else(|| format!("repository {repository} is not owner/name"))
        })
        .transpose()?;
    Ok(GovernedRequest {
        action: tuple.to_string(),
        target,
        body,
        repository,
        manifest_version,
        edit_last,
    })
}

fn declared_body_value(
    value: &str,
    canonical: &Canonicalization,
    next: Option<&OsString>,
) -> Result<Option<(String, String)>, String> {
    for field in &canonical.body_fields {
        let long = format!("--{field}");
        let short = match field.as_str() {
            "body" => Some("-b"),
            "reaction" => Some("-r"),
            _ => None,
        };
        if value == long || short == Some(value) {
            let supplied = next
                .and_then(|arg| arg.to_str())
                .ok_or_else(|| format!("{value} requires a value"))?;
            return Ok(Some((field.clone(), supplied.to_string())));
        }
        if let Some(supplied) = value.strip_prefix(&(long + "=")) {
            return Ok(Some((field.clone(), supplied.to_string())));
        }

        // GitHub CLI supports --body-file/-F for commands that submit text
        // bodies. Read the file here so this shim keeps the request on its
        // governed path and avoids shell-quoting problems with long Markdown
        // passed as an inline argument.
        if field == "body" {
            let file = if value == "--body-file" || value == "-F" {
                Some(
                    next.and_then(|arg| arg.to_str())
                        .ok_or_else(|| format!("{value} requires a value"))?,
                )
            } else {
                value
                    .strip_prefix("--body-file=")
                    .or_else(|| value.strip_prefix("-F="))
                    .or_else(|| value.strip_prefix("-F"))
            };
            if let Some(file) = file {
                let supplied =
                    read_body_file(Path::new(file)).map_err(|error| format!("{value}: {error}"))?;
                return Ok(Some((field.clone(), supplied)));
            }
        }
    }
    Ok(None)
}

fn read_body_file(path: &Path) -> Result<String, String> {
    let mut stdin = io::stdin().lock();
    read_body_file_from(path, &mut stdin)
}

fn read_body_file_from<R: Read>(path: &Path, stdin: &mut R) -> Result<String, String> {
    let mut body = String::new();
    // When the path is '-', upstream gh reads the body from standard input.
    // Do the same here so callers can provide stdin through this shim instead
    // of bypassing its governed path.
    if path == Path::new("-") {
        stdin
            .read_to_string(&mut body)
            .map_err(|error| format!("could not read body from stdin: {error}"))?;
    } else {
        body = fs::read_to_string(path)
            .map_err(|error| format!("could not read body file {}: {error}", path.display()))?;
    }
    Ok(body)
}

fn declared_review_event(value: &str) -> Option<&'static str> {
    match value {
        "--approve" => Some("APPROVE"),
        "--comment" => Some("COMMENT"),
        "--request-changes" => Some("REQUEST_CHANGES"),
        _ => None,
    }
}

fn explicit_repo(args: &[OsString]) -> Option<String> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let value = arg.to_str()?;
        if value == "--repo" || value == "-R" {
            return args.next()?.to_str().map(str::to_string);
        }
        if let Some(repository) = value.strip_prefix("--repo=") {
            return Some(repository.to_string());
        }
    }
    None
}

fn infer_repository_from_git() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    canonical_repository_key(&origin_remote(&cwd)?)
}

#[derive(Debug)]
enum RouteOutcome {
    Result(String),
    UpstreamError(String),
    Refusal(String),
    UnboundIdentity,
    SchemaMismatch(String),
    GovernanceUnavailable,
    Unavailable(String),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct SeamState {
    bound_holder: Option<String>,
    agent_binding: Option<AgentBinding>,
    last_seam_refusal: Option<LastSeamRefusal>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LastSeamRefusal {
    code: String,
    at_unix_secs: u64,
}

fn route_governed(
    paths: &StatePaths,
    determination: &RungRecord,
    agent_binding: &AgentBinding,
    request: GovernedRequest,
    now: u64,
) -> RouteOutcome {
    if let Err(error) = write_seam_state(paths, governed_seam_state(paths, None, agent_binding)) {
        return RouteOutcome::Unavailable(format!("governed self-report update failed: {error}"));
    }

    let Some(connection_file) = configured_connection_file() else {
        return RouteOutcome::GovernanceUnavailable;
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_root = project_root_for(&cwd);
    let record_paths = paths.clone();
    let agent_binding = agent_binding.clone();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return RouteOutcome::Unavailable(error.to_string()),
    };
    runtime
        .block_on(async move {
            let options = ConsumerOptions {
                call_timeout: Duration::from_secs(5),
                ..ConsumerOptions::default()
            };
            let consumer = SubcConsumer::connect(&connection_file, options)
                .await
                .map_err(|_| RouteOutcome::GovernanceUnavailable)?;
            let catalog = consumer
                .catalog_list()
                .await
                .map_err(|_| RouteOutcome::GovernanceUnavailable)?;
            let holder = route_holder(&catalog.modules);
            record_unexpected_gh_route_advertisers(&record_paths, &holder.unexpected_advertisers);
            let module_id = holder
                .module_id
                .ok_or(RouteOutcome::GovernanceUnavailable)?;
            let route = consumer
                .open_route(
                    RouteTarget::ManagementSurface {
                        module_id: module_id.clone(),
                    },
                    BindIdentity {
                        project_root: project_root.to_string_lossy().into_owned().into(),
                        harness: "aft-gh-shim".to_string(),
                        session: gh_session_id(&agent_binding.agent_id),
                    },
                    CallOptions::default(),
                )
                .await
                .map_err(|_| RouteOutcome::UnboundIdentity)?;
            if let Err(error) = write_seam_state(
                &record_paths,
                governed_seam_state(&record_paths, Some(module_id.clone()), &agent_binding),
            ) {
                let _ = consumer
                    .close_handle(&route, CloseRouteOptions::default())
                    .await;
                return Err(RouteOutcome::Unavailable(format!(
                    "governed self-report update failed: {error}"
                )));
            }
            let wire_request =
                governed_wire_request(determination, &agent_binding.agent_id, request);
            let body = serde_json::to_vec(&wire_request)
                .map_err(|error| RouteOutcome::SchemaMismatch(error.to_string()))?;
            let response = consumer
                .request(&route, body, CallOptions::default())
                .await
                .map_err(|error| RouteOutcome::Unavailable(error.to_string()));
            let _ = consumer
                .close_handle(&route, CloseRouteOptions::default())
                .await;
            let response = response?;
            let outcome = parse_governed_response(&response)?;
            if let RouteOutcome::Refusal(code) = &outcome {
                write_seam_state(
                    &record_paths,
                    SeamState {
                        bound_holder: Some(module_id),
                        agent_binding: Some(agent_binding),
                        last_seam_refusal: Some(LastSeamRefusal {
                            code: code.clone(),
                            at_unix_secs: now,
                        }),
                    },
                )
                .map_err(|error| {
                    RouteOutcome::Unavailable(format!(
                        "governed self-report update failed: {error}"
                    ))
                })?;
            }
            Ok(outcome)
        })
        .unwrap_or_else(|outcome| outcome)
}

fn refuse_governance_unavailable(
    paths: &StatePaths,
    agent_binding: &AgentBinding,
    now: u64,
) -> i32 {
    let state = SeamState {
        bound_holder: None,
        agent_binding: Some(agent_binding.clone()),
        last_seam_refusal: Some(LastSeamRefusal {
            code: RefusalCode::GovernanceUnavailable.as_str().to_string(),
            at_unix_secs: now,
        }),
    };
    if let Err(error) = write_seam_state(paths, state) {
        return refuse(
            RefusalCode::SeamUnavailable,
            &format!("governed self-report update failed: {error}"),
        );
    }
    refuse(
        RefusalCode::GovernanceUnavailable,
        GOVERNANCE_UNAVAILABLE_TEXT,
    )
}

fn governed_seam_state(
    paths: &StatePaths,
    bound_holder: Option<String>,
    agent_binding: &AgentBinding,
) -> SeamState {
    SeamState {
        bound_holder,
        agent_binding: Some(agent_binding.clone()),
        // A successful route is not a refusal event, so it must retain the last
        // holder refusal for operators to inspect its timestamp and code.
        last_seam_refusal: seam_state(paths).last_seam_refusal,
    }
}

fn write_seam_state(paths: &StatePaths, state: SeamState) -> io::Result<()> {
    fs::create_dir_all(&paths.root)?;
    let bytes = serde_json::to_vec(&state).map_err(io::Error::other)?;
    let temporary = paths.seam_state.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    // A governed result is visible only after its self-report transition is
    // durable enough to survive a process exit. Failure stays on the seam path
    // and is surfaced as a refusal instead of falling through to real `gh`.
    file.sync_data()?;
    fs::rename(temporary, &paths.seam_state)
}

fn seam_state(paths: &StatePaths) -> SeamState {
    fs::read(&paths.seam_state)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn governed_wire_request(
    determination: &RungRecord,
    agent_id: &str,
    request: GovernedRequest,
) -> Value {
    let edit_last = request.edit_last;
    let mut wire = json!({
        "operation": ROUTING_OPERATION,
        "gh_route_schema": 1,
        "action": request.action,
        "target": request.target,
        "body": request.body,
        "repository": request.repository,
        "manifest_version": request.manifest_version,
        "rung_as_of_unix_secs": determination.as_of_unix_secs,
        "metadata": {
            "agent_id": agent_id,
            "pid": std::process::id(),
        },
    });
    // Keep the create wire shape byte-for-byte compatible. The explicit marker
    // lets the route holder perform the same authenticated-user-only mutation
    // that gh's native --edit-last flag requests.
    if edit_last {
        wire["edit_last"] = Value::Bool(true);
    }
    wire
}

fn parse_governed_response(bytes: &[u8]) -> Result<RouteOutcome, RouteOutcome> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| {
        RouteOutcome::SchemaMismatch(
            "governance seam returned malformed or non-UTF-8 JSON".to_string(),
        )
    })?;
    let object = value.as_object().ok_or_else(|| {
        RouteOutcome::SchemaMismatch("governance seam response must be an object".to_string())
    })?;
    match object.get("outcome").and_then(Value::as_str) {
        Some("result") => {
            let schema = object
                .get("gh_route_schema")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    RouteOutcome::SchemaMismatch(
                        "governance seam omitted gh_route_schema".to_string(),
                    )
                })?;
            if schema > 1 {
                return Err(RouteOutcome::SchemaMismatch(format!(
                    "governance seam schema {schema} is newer than supported schema 1"
                )));
            }
            let result = object.get("result").ok_or_else(|| {
                RouteOutcome::SchemaMismatch("governance seam omitted result".to_string())
            })?;
            if let Some(body) = upstream_error_body(object, result) {
                return Ok(RouteOutcome::UpstreamError(body));
            }
            let field_order = object
                .get("field_order")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    RouteOutcome::SchemaMismatch("governance seam omitted field_order".to_string())
                })?;
            render_governed_response(result, field_order).map(RouteOutcome::Result)
        }
        Some("refusal") => {
            let refusal_code = object
                .get("refusal_code")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    RouteOutcome::SchemaMismatch(
                        "governance refusal omitted a string refusal_code".to_string(),
                    )
                })?;
            Ok(RouteOutcome::Refusal(refusal_code.to_string()))
        }
        Some("unbound_identity") => Ok(RouteOutcome::UnboundIdentity),
        _ => Err(RouteOutcome::SchemaMismatch(
            "governance seam returned an unknown outcome".to_string(),
        )),
    }
}

fn upstream_error_body(response: &Map<String, Value>, result: &Value) -> Option<String> {
    let result_object = result.as_object();
    let status = response
        .get("status")
        .or_else(|| response.get("status_code"))
        .or_else(|| result_object.and_then(|object| object.get("status")))
        .or_else(|| result_object.and_then(|object| object.get("status_code")))
        .and_then(|value| value.as_u64())?;
    if (200..300).contains(&status) {
        return None;
    }
    let body = response
        .get("error")
        .or_else(|| response.get("body"))
        .or_else(|| result_object.and_then(|object| object.get("error")))
        .or_else(|| result_object.and_then(|object| object.get("body")))
        .unwrap_or(result);
    Some(match body {
        Value::String(body) => body.clone(),
        _ => serde_json::to_string(body).unwrap_or_else(|_| body.to_string()),
    })
}

fn render_governed_response(result: &Value, field_order: &[Value]) -> Result<String, RouteOutcome> {
    let object = result.as_object().ok_or_else(|| {
        RouteOutcome::SchemaMismatch("governance result must be an object".to_string())
    })?;
    let mut output = String::new();
    let mut rendered = BTreeSet::new();
    for field in field_order {
        let field = field.as_str().ok_or_else(|| {
            RouteOutcome::SchemaMismatch("field_order must contain string fields".to_string())
        })?;
        let value = object.get(field).ok_or_else(|| {
            RouteOutcome::SchemaMismatch(format!(
                "field_order references absent result field {field}"
            ))
        })?;
        if !rendered.insert(field) {
            return Err(RouteOutcome::SchemaMismatch(format!(
                "field_order repeats result field {field}"
            )));
        }
        render_field(&mut output, field, value)?;
    }
    if rendered.len() != object.len() {
        return Err(RouteOutcome::SchemaMismatch(
            "field_order does not cover every governed result field".to_string(),
        ));
    }
    Ok(output)
}

fn render_field(output: &mut String, field: &str, value: &Value) -> Result<(), RouteOutcome> {
    match value {
        Value::Array(values) => {
            output.push_str(field);
            output.push_str(":\n");
            for value in values {
                output.push_str("  ");
                output.push_str(&render_scalar(value)?);
                output.push('\n');
            }
        }
        _ => {
            output.push_str(field);
            output.push_str(": ");
            output.push_str(&render_scalar(value)?);
            output.push('\n');
        }
    }
    Ok(())
}

fn render_scalar(value: &Value) -> Result<String, RouteOutcome> {
    match value {
        Value::String(value) => serde_json::to_string(value)
            .map_err(|error| RouteOutcome::SchemaMismatch(error.to_string())),
        Value::Number(_) | Value::Bool(_) | Value::Null => Ok(value.to_string()),
        Value::Object(_) | Value::Array(_) => serde_json::to_string(value)
            .map_err(|error| RouteOutcome::SchemaMismatch(error.to_string())),
    }
}

fn append_bypass_audit(
    paths: &StatePaths,
    tuple: &str,
    repository: Option<&str>,
    now: u64,
) -> io::Result<()> {
    fs::create_dir_all(&paths.root)?;
    let mut record = serde_json::to_vec(&json!({
        "as_of_unix_secs": now,
        "tuple": tuple,
        "repository": repository,
    }))
    .map_err(io::Error::other)?;
    record.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.bypass_audit)?;
    file.write_all(&record)?;
    // An operator bypass is allowed only after the audit record is durable enough
    // to survive a process replacement. If this returns an error we do not exec.
    file.sync_data()
}

#[derive(Serialize)]
struct SelfReport {
    shim_version: &'static str,
    gh_routing_schema_floor: u64,
    unexpected_gh_route_advertiser: Option<Vec<String>>,
    bound_holder: Option<String>,
    agent_binding: Option<AgentBinding>,
    last_seam_refusal: Option<LastSeamRefusal>,
    cached_manifest: CachedManifestReport,
    last_rung: LastRungReport,
    bypass_audit: Option<Vec<Value>>,
    bypass_audit_error: Option<String>,
    executing_image: Option<String>,
    executing_image_error: Option<String>,
    real_gh_resolution: Option<RealGhResolution>,
    real_gh_resolution_error: Option<String>,
}

#[derive(Serialize)]
struct CachedManifestReport {
    version: Option<u64>,
    /// Signed provenance metadata for the manifest used by this report; it does
    /// not control artifact validity after signature verification.
    issued_at_unix_secs: Option<u64>,
    /// The compiled trust-set key that verified the installed envelope.
    verified_by_key_id: Option<String>,
    /// Key identifiers compiled into this executing image's manifest trust set.
    compiled_trust_set_key_ids: Vec<&'static str>,
    version_error: Option<String>,
    state: Option<&'static str>,
    state_error: Option<String>,
    diagnostics: Vec<&'static str>,
    diagnostic_guidance: Option<&'static str>,
}

#[derive(Serialize)]
struct LastRungReport {
    rung: Option<&'static str>,
    rung_error: Option<String>,
    as_of_unix_secs: Option<u64>,
    as_of_unix_secs_error: Option<String>,
    determination_inputs: Option<BTreeMap<String, String>>,
    determination_inputs_error: Option<String>,
    recorded_by_image_path: Option<String>,
    recorded_by_version: Option<String>,
    recorded_by_repo_key: Option<String>,
}

#[derive(Serialize)]
struct RealGhResolution {
    path: String,
    shim_path_positions: Vec<usize>,
}

fn print_self_report(paths: &StatePaths) {
    // This is deliberately one JSON document, rather than status lines, so a
    // later forensic process can consume it with jq while every dependency is down.
    if let Ok(document) = render_self_report(paths) {
        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(document.as_bytes());
    }
}

fn render_self_report(paths: &StatePaths) -> Result<String, serde_json::Error> {
    let report = build_self_report(paths);
    let mut document = serde_json::to_string(&report)?;
    document.push('\n');
    Ok(document)
}

fn build_self_report(paths: &StatePaths) -> SelfReport {
    let image = self_report_executing_image();
    let (real_gh_resolution, real_gh_resolution_error) = match image.as_ref() {
        Ok(image) => match resolve_real_gh(image) {
            Some(path) => (
                Some(RealGhResolution {
                    path: path.to_string_lossy().into_owned(),
                    shim_path_positions: executing_image_path_positions(image),
                }),
                None,
            ),
            None => (
                None,
                Some(
                    "PATH contains no upstream gh after skipping the executing shim image"
                        .to_string(),
                ),
            ),
        },
        Err(error) => (None, Some(format!("executing image unavailable: {error}"))),
    };
    let (bypass_audit, bypass_audit_error) = read_bypass_audit(paths);
    let seam_state = seam_state(paths);
    // When the operator hard-off is set, the shim is byte-transparent passthrough
    // and never probes the daemon or catalog, so the status report reflects that
    // disabled determination instead of whatever stale rung/manifest cache exists.
    let disabled = gh_shim_enabled_from_config_doc(read_user_config_doc().as_deref().unwrap_or(""))
        == Some(false);
    let (cached_manifest, last_rung) = if disabled {
        (disabled_manifest_report(), disabled_last_rung_report())
    } else {
        (cached_manifest_report(paths), last_rung_report(paths))
    };
    SelfReport {
        shim_version: env!("CARGO_PKG_VERSION"),
        gh_routing_schema_floor: SCHEMA_FLOOR,
        unexpected_gh_route_advertiser: unexpected_gh_route_advertisers(paths),
        bound_holder: seam_state.bound_holder,
        agent_binding: seam_state.agent_binding,
        last_seam_refusal: seam_state.last_seam_refusal,
        cached_manifest,
        last_rung,
        bypass_audit,
        bypass_audit_error,
        executing_image: image
            .as_ref()
            .ok()
            .map(|path| path.to_string_lossy().into_owned()),
        executing_image_error: image.err(),
        real_gh_resolution,
        real_gh_resolution_error,
    }
}

/// Self-report for the disabled-by-config state: the shim is a hard passthrough
/// and never consults the manifest, so the cached-manifest slot reports that
/// disabled state rather than a stale on-disk manifest.
fn disabled_manifest_report() -> CachedManifestReport {
    CachedManifestReport {
        version: None,
        issued_at_unix_secs: None,
        verified_by_key_id: None,
        compiled_trust_set_key_ids: trust_set_key_ids(compiled_manifest_trust_set()),
        version_error: None,
        state: Some("disabled"),
        state_error: None,
        diagnostics: Vec::new(),
        diagnostic_guidance: None,
    }
}

/// Self-report for the disabled-by-config state: R1 passthrough with the
/// disabled determination input, matching what `determine_rung` would produce.
fn disabled_last_rung_report() -> LastRungReport {
    LastRungReport {
        rung: Some(Rung::R1.label()),
        rung_error: None,
        as_of_unix_secs: Some(unix_seconds()),
        as_of_unix_secs_error: None,
        determination_inputs: Some(BTreeMap::from([(
            "connection_file".to_string(),
            "disabled_by_config".to_string(),
        )])),
        determination_inputs_error: None,
        recorded_by_image_path: None,
        recorded_by_version: None,
        recorded_by_repo_key: None,
    }
}

fn cached_manifest_report(paths: &StatePaths) -> CachedManifestReport {
    cached_manifest_report_at(paths, unix_seconds())
}

fn cached_manifest_report_at(paths: &StatePaths, now: u64) -> CachedManifestReport {
    cached_manifest_report_at_with(paths, now, compiled_manifest_trust_set())
}

fn cached_manifest_report_at_with(
    paths: &StatePaths,
    now: u64,
    trust_set: &[Option<ManifestTrustKey>],
) -> CachedManifestReport {
    let compiled_trust_set_key_ids = trust_set_key_ids(trust_set);
    match load_manifest_with_trust_set(paths, now, trust_set) {
        Ok(verified) => CachedManifestReport {
            version: Some(verified.manifest.manifest_version),
            issued_at_unix_secs: Some(verified.manifest.issued_at_unix_secs),
            verified_by_key_id: Some(verified.verified_by_key_id),
            compiled_trust_set_key_ids,
            version_error: None,
            state: Some("valid"),
            state_error: None,
            diagnostics: Vec::new(),
            diagnostic_guidance: None,
        },
        Err(ManifestProblem::Missing) => {
            let error = ManifestProblem::Missing.status_label();
            CachedManifestReport {
                version: None,
                issued_at_unix_secs: None,
                verified_by_key_id: None,
                compiled_trust_set_key_ids,
                version_error: Some(error.clone()),
                state: None,
                state_error: Some(error),
                diagnostics: vec![SelfReportDiagnostic::ManifestUnavailable.as_str()],
                diagnostic_guidance: None,
            }
        }
        Err(problem) => {
            // Artifact present but failing. The regressed-manifest arm is loud
            // in self-report: name the arm state first, then the artifact
            // fault that triggered it.
            let diagnostic_guidance = problem.untrusted_manifest_key_steering();
            match read_last_valid_manifest(paths) {
                Some(cache) => CachedManifestReport {
                    version: Some(cache.manifest.manifest_version),
                    issued_at_unix_secs: Some(cache.manifest.issued_at_unix_secs),
                    verified_by_key_id: None,
                    compiled_trust_set_key_ids,
                    version_error: None,
                    state: Some("regressed"),
                    state_error: None,
                    diagnostics: vec![
                        SelfReportDiagnostic::ManifestRegressed.as_str(),
                        problem.diagnostic().as_str(),
                    ],
                    diagnostic_guidance,
                },
                None => {
                    let error = problem.status_label();
                    CachedManifestReport {
                        version: None,
                        issued_at_unix_secs: None,
                        verified_by_key_id: None,
                        compiled_trust_set_key_ids,
                        version_error: Some(error.clone()),
                        state: None,
                        state_error: Some(error),
                        diagnostics: vec![problem.diagnostic().as_str()],
                        diagnostic_guidance,
                    }
                }
            }
        }
    }
}

fn last_rung_report(paths: &StatePaths) -> LastRungReport {
    match fs::read(&paths.rung) {
        Ok(bytes) => match serde_json::from_slice::<RungRecord>(&bytes) {
            Ok(record) => LastRungReport {
                rung: Some(record.rung.label()),
                rung_error: None,
                as_of_unix_secs: Some(record.as_of_unix_secs),
                as_of_unix_secs_error: None,
                determination_inputs: Some(record.inputs),
                determination_inputs_error: None,
                recorded_by_image_path: Some(
                    record
                        .recorded_by_image_path
                        .unwrap_or_else(|| PRE_PROVENANCE_RECORD.to_string()),
                ),
                recorded_by_version: Some(
                    record
                        .recorded_by_version
                        .unwrap_or_else(|| PRE_PROVENANCE_RECORD.to_string()),
                ),
                recorded_by_repo_key: Some(
                    record
                        .recorded_by_repo_key
                        .unwrap_or_else(|| PRE_PROVENANCE_RECORD.to_string()),
                ),
            },
            Err(error) => unavailable_last_rung(format!("corrupt rung cache: {error}")),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            unavailable_last_rung("rung cache is unavailable".to_string())
        }
        Err(error) => unavailable_last_rung(format!("rung cache is unavailable: {error}")),
    }
}

fn unavailable_last_rung(error: String) -> LastRungReport {
    LastRungReport {
        rung: None,
        rung_error: Some(error.clone()),
        as_of_unix_secs: None,
        as_of_unix_secs_error: Some(error.clone()),
        determination_inputs: None,
        determination_inputs_error: Some(error),
        recorded_by_image_path: None,
        recorded_by_version: None,
        recorded_by_repo_key: None,
    }
}

fn read_bypass_audit(paths: &StatePaths) -> (Option<Vec<Value>>, Option<String>) {
    let contents = match fs::read_to_string(&paths.bypass_audit) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return (Some(Vec::new()), None),
        Err(error) => return (None, Some(format!("bypass audit is unavailable: {error}"))),
    };
    let mut records = Vec::new();
    for (line_number, line) in contents.lines().enumerate() {
        match serde_json::from_str(line) {
            Ok(record) => records.push(record),
            Err(error) => {
                return (
                    None,
                    Some(format!(
                        "bypass audit is corrupt at line {}: {error}",
                        line_number + 1
                    )),
                )
            }
        }
    }
    (Some(records), None)
}

fn unexpected_gh_route_advertisers(paths: &StatePaths) -> Option<Vec<String>> {
    serde_json::from_slice(&fs::read(&paths.unexpected_gh_route_advertisers).ok()?)
        .ok()
        .filter(|advertisers: &Vec<String>| !advertisers.is_empty())
}

fn record_unexpected_gh_route_advertisers(paths: &StatePaths, advertisers: &[String]) {
    if advertisers.is_empty() {
        return;
    }
    let mut recorded = unexpected_gh_route_advertisers(paths)
        .unwrap_or_default()
        .into_iter()
        .collect::<BTreeSet<_>>();
    recorded.extend(advertisers.iter().cloned());
    let Ok(bytes) = serde_json::to_vec(&recorded.into_iter().collect::<Vec<_>>()) else {
        return;
    };
    let _ = fs::create_dir_all(&paths.root);
    let temporary = paths.unexpected_gh_route_advertisers.with_extension("tmp");
    if fs::write(&temporary, bytes).is_ok() {
        let _ = fs::rename(temporary, &paths.unexpected_gh_route_advertisers);
    }
}

fn self_report_executing_image() -> Result<PathBuf, String> {
    let path = std::env::current_exe().map_err(|error| error.to_string())?;
    Ok(path.canonicalize().unwrap_or(path))
}

fn executing_image() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok().or(Some(path)))
        .unwrap_or_else(|| PathBuf::from("unavailable"))
}

fn executing_image_path_positions(image: &Path) -> Vec<usize> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .enumerate()
        .filter_map(|(index, directory)| same_image(&directory.join("gh"), image).then_some(index))
        .collect()
}

fn delegate(args: &[OsString]) -> i32 {
    let image = executing_image();
    let Some(real_gh) = resolve_real_gh(&image) else {
        return refuse(
            RefusalCode::NoRealGh,
            "PATH contains no upstream gh after skipping the executing shim image",
        );
    };
    exec_real_gh(real_gh, args)
}

fn resolve_real_gh(executing_image: &Path) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let shims_dir = std::env::var_os("AFT_GH_SHIMS_DIR").map(PathBuf::from);
    resolve_real_gh_in_path(executing_image, &path, shims_dir.as_deref())
}

fn resolve_real_gh_in_path(
    executing_image: &Path,
    path: &OsStr,
    shims_dir: Option<&Path>,
) -> Option<PathBuf> {
    std::env::split_paths(path).find_map(|directory| {
        if shims_dir.is_some_and(|shims_dir| same_directory(&directory, shims_dir)) {
            return None;
        }
        gh_candidate_names().iter().find_map(|name| {
            let candidate = directory.join(name);
            (is_executable_file(&candidate) && !same_image(&candidate, executing_image))
                .then_some(candidate)
        })
    })
}

#[cfg(windows)]
fn gh_candidate_names() -> &'static [&'static str] {
    &["gh.exe", "gh.cmd", "gh.bat", "gh"]
}

#[cfg(not(windows))]
fn gh_candidate_names() -> &'static [&'static str] {
    &["gh"]
}

fn same_directory(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0);
    }
    #[cfg(not(unix))]
    true
}

fn same_image(left: &Path, right: &Path) -> bool {
    let left_canonical = left.canonicalize().ok();
    let right_canonical = right.canonicalize().ok();
    if left_canonical.is_some() && left_canonical == right_canonical {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(left), Ok(right)) = (fs::metadata(left), fs::metadata(right)) {
            return left.dev() == right.dev() && left.ino() == right.ino();
        }
    }
    false
}

#[cfg(unix)]
fn exec_real_gh(real_gh: PathBuf, args: &[OsString]) -> i32 {
    use std::os::unix::process::CommandExt;
    let error = Command::new(real_gh).args(args).exec();
    // `exec` returns only if a candidate disappeared after the PATH scan. This
    // remains a shim refusal, rather than silently treating a failed exec as a
    // successful no-op.
    refuse(
        RefusalCode::NoRealGh,
        &format!("unable to exec upstream gh: {error}"),
    )
}

#[cfg(not(unix))]
fn exec_real_gh(real_gh: PathBuf, args: &[OsString]) -> i32 {
    match Command::new(real_gh).args(args).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => refuse(
            RefusalCode::NoRealGh,
            &format!("unable to exec upstream gh: {error}"),
        ),
    }
}

fn refuse(code: RefusalCode, text: &str) -> i32 {
    let text = text.replace(['\n', '\r'], " ");
    eprintln!("gh-shim: {}: {text}", code.as_str());
    REFUSAL_EXIT_STATUS
}

fn current_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unsupported"
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use sha2::{Digest, Sha256};

    const TEST_SEED: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];
    /// Seed for the standby-slot fixture key. Test-only material: the compiled
    /// dev trust set keeps exactly one key, and this second keypair exists so
    /// the two-slot trust-set mechanics (standby accepted, unknown refused)
    /// can be exercised against an injected set.
    const STANDBY_TEST_SEED: [u8; 32] = *b"gh-shim-standby-fixture-seed-001";
    const DEV_STANDBY_MANIFEST_KEY_ID: &str = "gh-routing-dev-standby-key-v1";
    /// Issue time baked into the canonical manifest fixture; test clocks and
    /// signed provenance variants are expressed relative to this metadata.
    const FIXTURE_ISSUED_AT: u64 = 1_787_184_000;
    const TEST_NOW: u64 = FIXTURE_ISSUED_AT + 60;
    const FIXTURE_ACCEPTED_SEAM_REFUSAL_CODES: &[&str] = &[
        "identity_mismatch",
        "unmapped_operation",
        "custody_unavailable",
        "schema_unsupported",
        "rate_limited",
    ];

    fn fixture_manifest() -> Manifest {
        serde_json::from_str(include_str!(
            "../tests/fixtures/gh_shim/initial-manifest-v1.json"
        ))
        .expect("initial manifest fixture")
    }

    fn v9_fixture_manifest() -> Manifest {
        serde_json::from_str(include_str!("../tests/fixtures/gh_shim/v9-manifest.json"))
            .expect("v9 manifest fixture")
    }

    fn v10_fixture_manifest() -> Manifest {
        serde_json::from_str(include_str!("../tests/fixtures/gh_shim/v10-manifest.json"))
            .expect("v10 manifest fixture")
    }

    fn edit_last_vectors_fixture() -> Value {
        // JSON has no comment syntax, so strip the human-readable provenance
        // header before parsing the copied producer fixture.
        let fixture = include_str!("../tests/fixtures/gh_shim/edit-last-vectors-v1.json");
        let json = fixture
            .lines()
            .filter(|line| !line.starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        serde_json::from_str(&json).expect("producer edit-last vectors fixture")
    }

    fn signed_with(
        manifest: &Manifest,
        fetched_at_unix_secs: u64,
        seed: &[u8; 32],
        key_id: &str,
    ) -> SignedManifest {
        let key = Ed25519KeyPair::from_seed_unchecked(seed).expect("test key");
        let bytes = serde_json::to_vec(manifest).expect("manifest bytes");
        SignedManifest {
            artifact_id: MANIFEST_ARTIFACT_ID.to_string(),
            envelope_version: ENVELOPE_VERSION,
            key_id: key_id.to_string(),
            fetched_at_unix_secs,
            signature: base64::engine::general_purpose::STANDARD.encode(key.sign(&bytes).as_ref()),
            manifest_bytes: String::from_utf8(bytes).expect("manifest bytes are UTF-8"),
        }
    }

    fn signed(manifest: &Manifest, fetched_at_unix_secs: u64) -> SignedManifest {
        let key = Ed25519KeyPair::from_seed_unchecked(&TEST_SEED).expect("test key");
        assert_eq!(key.public_key().as_ref(), DEV_MANIFEST_PUBLIC_KEY);
        signed_with(
            manifest,
            fetched_at_unix_secs,
            &TEST_SEED,
            DEV_MANIFEST_KEY_ID,
        )
    }

    fn write_signed_manifest(paths: &StatePaths, manifest: Manifest, now: u64) {
        fs::create_dir_all(&paths.root).expect("state root");
        fs::write(
            &paths.manifest,
            serde_json::to_vec(&signed(&manifest, now)).expect("signed manifest"),
        )
        .expect("manifest cache");
    }

    fn write_envelope_fixture(paths: &StatePaths, envelope_json: &str) {
        fs::create_dir_all(&paths.root).expect("state root");
        fs::write(&paths.manifest, envelope_json.as_bytes()).expect("manifest cache");
    }

    fn test_rung_provenance() -> RungRecordProvenance {
        RungRecordProvenance {
            image_path: "/opt/cortexkit/aft-gh-shim".to_string(),
            version: "0.53.0-test".to_string(),
            repo_key: "cortexkit/aft".to_string(),
        }
    }

    #[test]
    fn shim_dispatch_precedes_global_argument_scans_for_both_forms() {
        assert!(is_shim_invocation(
            OsStr::new("gh"),
            &[OsString::from("--version")]
        ));
        assert!(is_shim_invocation(
            OsStr::new("aft"),
            &[OsString::from("gh-shim"), OsString::from("--version")]
        ));
        assert!(!is_shim_invocation(
            OsStr::new("aft"),
            &[OsString::from("--version")]
        ));
    }

    #[test]
    fn reserved_self_report_tokens_are_exactly_the_two_first_arguments() {
        assert_eq!(RESERVED_SELF_REPORT, ["--status", "--shim-version"]);
        assert!(is_reserved_self_report(&[OsString::from("--status")]));
        assert!(is_reserved_self_report(&[OsString::from("--shim-version")]));
        assert!(!is_reserved_self_report(&[OsString::from("status")]));
        assert!(!is_reserved_self_report(&[
            OsString::from("issue"),
            OsString::from("--status")
        ]));
    }

    #[cfg(unix)]
    #[test]
    fn real_gh_resolution_skips_the_managed_shims_directory_without_recursing() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("aft");
        fs::write(&image, "image").unwrap();
        let shims = directory.path().join("shims");
        let upstream = directory.path().join("upstream");
        fs::create_dir_all(&shims).unwrap();
        fs::create_dir_all(&upstream).unwrap();
        symlink(&image, shims.join("gh")).unwrap();
        let real = upstream.join("gh");
        fs::write(&real, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&real).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&real, permissions).unwrap();
        let path = std::env::join_paths([shims.clone(), upstream]).unwrap();

        assert_eq!(
            resolve_real_gh_in_path(&image, &path, Some(&shims)),
            Some(real)
        );
    }

    #[test]
    fn status_serializes_one_json_document_with_the_exact_top_level_schema() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let document = render_self_report(&paths).expect("self report serialization");
        assert!(document.ends_with('\n'));
        let value: Value = serde_json::from_str(&document).expect("self report JSON");
        let keys = value
            .as_object()
            .expect("self report object")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                "shim_version",
                "gh_routing_schema_floor",
                "unexpected_gh_route_advertiser",
                "bound_holder",
                "agent_binding",
                "last_seam_refusal",
                "cached_manifest",
                "last_rung",
                "bypass_audit",
                "bypass_audit_error",
                "executing_image",
                "executing_image_error",
                "real_gh_resolution",
                "real_gh_resolution_error",
            ]
        );
    }

    #[test]
    fn route_holder_is_pinned_and_records_other_advertisers() {
        let holder = select_route_holder([
            "other-module".to_string(),
            ROUTING_HOLDER_MODULE_ID.to_string(),
            "another-module".to_string(),
        ]);
        assert_eq!(holder.module_id.as_deref(), Some(ROUTING_HOLDER_MODULE_ID));
        assert_eq!(
            holder.unexpected_advertisers,
            vec!["another-module", "other-module"]
        );

        let holder = select_route_holder(["other-module".to_string()]);
        assert_eq!(holder.module_id, None);
        assert_eq!(holder.unexpected_advertisers, vec!["other-module"]);
    }

    #[test]
    fn unexpected_route_advertisers_are_persisted_for_self_report() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        record_unexpected_gh_route_advertisers(&paths, &["other-module".to_string()]);
        record_unexpected_gh_route_advertisers(&paths, &["another-module".to_string()]);

        assert_eq!(
            unexpected_gh_route_advertisers(&paths),
            Some(vec![
                "another-module".to_string(),
                "other-module".to_string(),
            ])
        );
        assert_eq!(
            build_self_report(&paths).unexpected_gh_route_advertiser,
            Some(vec![
                "another-module".to_string(),
                "other-module".to_string(),
            ])
        );
    }

    #[test]
    fn disabled_by_config_short_circuits_to_r1_without_connection_file_read() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        // A disabled shim must resolve R1 with the named reason even when a
        // connection file is configured, and must not touch the daemon/catalog.
        let doc = serde_json::json!({
            "gh_shim": { "enabled": false },
            "subc": { "connection_file": "/nonexistent/connection.json" }
        })
        .to_string();
        let record = determine_rung_from_doc(
            &paths,
            Path::new("/cwd"),
            123,
            std::time::Instant::now() + DISCOVERY_BUDGET,
            Some(&doc),
        );
        assert_eq!(record.record.rung, Rung::R1);
        assert_eq!(
            record
                .record
                .inputs
                .get("connection_file")
                .map(String::as_str),
            Some("disabled_by_config")
        );
        // R1 is never written durably.
        assert!(!paths.root.join("rung-cache.json").exists());
    }

    #[test]
    fn configured_but_unreachable_connection_file_is_distinct_from_absence() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let connection_file = directory.path().join("missing-connection.json");
        let doc = serde_json::json!({
            "subc": { "connection_file": connection_file }
        })
        .to_string();
        let record = determine_rung_from_doc(
            &paths,
            Path::new("/cwd"),
            1,
            std::time::Instant::now() + DISCOVERY_BUDGET,
            Some(&doc),
        );
        assert_eq!(record.record.rung, Rung::R1);
        assert_eq!(
            record
                .record
                .inputs
                .get("connection_file")
                .map(String::as_str),
            Some("unreachable")
        );
    }

    #[test]
    fn enabled_default_keeps_structural_rungs() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        // No gh_shim key (default true) and no connection file → structural R1.
        let record = determine_rung_from_doc(
            &paths,
            Path::new("/cwd"),
            1,
            std::time::Instant::now() + DISCOVERY_BUDGET,
            Some("{}"),
        );
        assert_eq!(record.record.rung, Rung::R1);
        assert_eq!(
            record
                .record
                .inputs
                .get("connection_file")
                .map(String::as_str),
            Some("absent_or_unparseable")
        );
    }

    #[test]
    fn xdg_connection_config_precedes_home_config() {
        let directory = tempfile::tempdir().unwrap();
        let xdg = directory.path().join("xdg");
        let home = directory.path().join("home");
        let xdg_connection = directory.path().join("xdg-connection.json");
        let home_connection = directory.path().join("home-connection.json");
        fs::write(&xdg_connection, "{}").unwrap();
        fs::write(&home_connection, "{}").unwrap();
        let xdg_config = xdg.join("cortexkit/aft.jsonc");
        let home_config = home.join(".config/cortexkit/aft.jsonc");
        fs::create_dir_all(xdg_config.parent().unwrap()).unwrap();
        fs::create_dir_all(home_config.parent().unwrap()).unwrap();
        // Serialize through serde_json so Windows backslash paths are
        // JSON-escaped; a raw format! of Path::display() writes `C:\Users\...`
        // into the string, which is invalid JSON and parses to None.
        fs::write(
            &xdg_config,
            serde_json::json!({"subc": {"connection_file": xdg_connection}}).to_string(),
        )
        .unwrap();
        fs::write(
            &home_config,
            serde_json::json!({"subc": {"connection_file": home_connection}}).to_string(),
        )
        .unwrap();

        assert_eq!(
            configured_connection_file_from(Some(xdg.as_os_str()), Some(home.as_os_str())),
            Some(xdg_connection)
        );
    }

    #[test]
    fn initial_manifest_is_complete_and_valid() {
        fixture_manifest()
            .validate()
            .expect("valid initial manifest");
    }

    #[test]
    fn v9_admin_tuple_fixture_differentiates_native_writes_from_raw_api_delete() {
        let manifest = v9_fixture_manifest();
        assert_eq!(manifest.manifest_version, 9);
        manifest.validate().expect("valid v9 manifest");

        for (args, expected_tuple) in [
            (
                vec![
                    OsString::from("repo"),
                    OsString::from("edit"),
                    OsString::from("cortexkit/insula"),
                    OsString::from("--visibility"),
                    OsString::from("public"),
                ],
                "repo edit",
            ),
            (
                vec![
                    OsString::from("repo"),
                    OsString::from("edit"),
                    OsString::from("cortexkit/insula"),
                    OsString::from("--visibility"),
                    OsString::from("private"),
                ],
                "repo edit",
            ),
            (
                vec![
                    OsString::from("run"),
                    OsString::from("delete"),
                    OsString::from("123"),
                    OsString::from("--repo"),
                    OsString::from("cortexkit/insula"),
                ],
                "run delete",
            ),
        ] {
            assert!(matches!(
                classify(&args, &manifest, "macos"),
                Classification::Admin { tuple } if tuple == expected_tuple
            ));
        }

        let raw_api_delete = [
            OsString::from("api"),
            OsString::from("-X"),
            OsString::from("DELETE"),
            OsString::from("repos/cortexkit/insula/actions/runs/123"),
        ];
        assert!(matches!(
            classify(&raw_api_delete, &manifest, "macos"),
            Classification::Unclassified
        ));

        let get_control = [
            OsString::from("api"),
            OsString::from("repos/cortexkit/insula"),
            OsString::from("--jq"),
            OsString::from(".name"),
        ];
        assert!(matches!(
            classify(&get_control, &manifest, "macos"),
            Classification::Mechanical
        ));
    }

    #[test]
    fn v10_workflow_run_admin_tuple_is_version_gated_and_raw_dispatch_stays_unclassified() {
        let manifest = v10_fixture_manifest();
        assert_eq!(manifest.manifest_version, 10);
        manifest.validate().expect("valid v10 manifest");

        let workflow_run = [
            OsString::from("workflow"),
            OsString::from("run"),
            OsString::from("ci.yml"),
            OsString::from("--ref"),
            OsString::from("main"),
        ];
        assert!(matches!(
            classify(&workflow_run, &manifest, "macos"),
            Classification::Admin { tuple } if tuple == "workflow run"
        ));

        // Keep the v10 declaration fields but set its manifest version to 9,
        // verifying that the classifier rejects v10-only declarations when the
        // manifest version is unsupported.
        let mut v9_manifest = manifest.clone();
        v9_manifest.manifest_version = 9;
        assert!(matches!(
            classify(&workflow_run, &v9_manifest, "macos"),
            Classification::Unclassified
        ));

        let raw_api_dispatch = [
            OsString::from("api"),
            OsString::from("-X"),
            OsString::from("POST"),
            OsString::from("repos/cortexkit/aft/actions/workflows/ci.yml/dispatches"),
        ];
        for manifest in [&manifest, &v9_manifest] {
            assert!(matches!(
                classify(&raw_api_dispatch, manifest, "macos"),
                Classification::Unclassified
            ));
        }
    }

    #[test]
    fn v10_run_rerun_is_flag_tolerant_and_run_cancel_stays_out_of_bypass_set() {
        let mut manifest = v10_fixture_manifest();
        let admin = manifest
            .tiers
            .get_mut(&Tier::Admin)
            .expect("v10 admin tier");
        for tuple in ["run rerun", "run cancel"] {
            admin.push(TupleDecl::Details {
                tuple: tuple.to_string(),
                platform: vec!["macos".to_string(), "linux".to_string()],
                api_match: None,
                rationale: None,
            });
        }
        manifest.validate().expect("valid v10 admin extensions");

        for args in [
            vec![
                OsString::from("run"),
                OsString::from("rerun"),
                OsString::from("123"),
                OsString::from("--failed"),
            ],
            vec![
                OsString::from("run"),
                OsString::from("rerun"),
                OsString::from("123"),
                OsString::from("--job"),
                OsString::from("17"),
            ],
        ] {
            assert!(matches!(
                classify(&args, &manifest, "macos"),
                Classification::Admin { tuple } if tuple == "run rerun"
            ));
        }
        assert!(is_reviewed_admin_tuple(10, "run rerun"));
        assert!(!is_reviewed_admin_tuple(9, "run rerun"));
        let mut v9_manifest = manifest.clone();
        v9_manifest.manifest_version = 9;
        let v9_rerun = [
            OsString::from("run"),
            OsString::from("rerun"),
            OsString::from("123"),
            OsString::from("--failed"),
        ];
        assert!(matches!(
            classify(&v9_rerun, &v9_manifest, "macos"),
            Classification::Unclassified
        ));

        let run_cancel = [
            OsString::from("run"),
            OsString::from("cancel"),
            OsString::from("123"),
        ];
        assert!(!is_reviewed_admin_tuple(10, "run cancel"));
        assert!(matches!(
            classify(&run_cancel, &manifest, "macos"),
            Classification::Unclassified
        ));
    }

    #[test]
    fn v10_edit_last_comment_variants_are_exactly_governed_and_author_scoped() {
        let manifest = v10_fixture_manifest();
        manifest.validate().expect("valid v10 manifest");

        for (verb, number) in [("issue", "42"), ("pr", "7")] {
            let args = [
                OsString::from(verb),
                OsString::from("comment"),
                OsString::from(number),
                OsString::from("--body"),
                OsString::from("replace the draft"),
                OsString::from("--edit-last"),
            ];
            let Classification::Governed { tuple, canonical } = classify(&args, &manifest, "macos")
            else {
                panic!("native edit-last should use the governed comment tuple: {args:?}");
            };
            assert_eq!(tuple, format!("{verb} comment"));

            let request =
                canonicalize_governed(&args, &tuple, &canonical, manifest.manifest_version)
                    .expect("reviewed edit-last form should canonicalize");
            assert!(request.edit_last);
            assert_eq!(request.target["number"], number);
            assert_eq!(request.body["body"], "replace the draft");

            let wire = governed_wire_request(
                &(RungDetermination::r3(1, manifest.manifest_version, &test_rung_provenance())
                    .record),
                "alfonso-aft",
                request,
            );
            assert_eq!(wire["edit_last"], true);
        }

        let bare_create = [
            OsString::from("issue"),
            OsString::from("comment"),
            OsString::from("42"),
            OsString::from("--body"),
            OsString::from("new comment"),
        ];
        let Classification::Governed { tuple, canonical } =
            classify(&bare_create, &manifest, "macos")
        else {
            panic!("bare comment creation must remain governed");
        };
        let request =
            canonicalize_governed(&bare_create, &tuple, &canonical, manifest.manifest_version)
                .expect("bare comment creation should remain canonicalizable");
        assert!(!request.edit_last);
        let wire = governed_wire_request(
            &(RungDetermination::r3(1, manifest.manifest_version, &test_rung_provenance()).record),
            "alfonso-aft",
            request,
        );
        assert!(wire.get("edit_last").is_none());

        // The edit-last allowlist is enforced starting with manifest version 10;
        // older signed manifests do not gain this mutation merely because they
        // contain the same tuple.
        let mut v9_manifest = manifest.clone();
        v9_manifest.manifest_version = 9;
        let v9_edit = [
            OsString::from("pr"),
            OsString::from("comment"),
            OsString::from("7"),
            OsString::from("--body"),
            OsString::from("replace the draft"),
            OsString::from("--edit-last"),
        ];
        assert!(matches!(
            classify(&v9_edit, &v9_manifest, "macos"),
            Classification::Unclassified
        ));

        // gh also exposes --delete-last, but deletion is not the
        // authenticated-user-only edit operation allowed by --edit-last, so this
        // flag must fail closed.
        let delete_last = [
            OsString::from("pr"),
            OsString::from("comment"),
            OsString::from("7"),
            OsString::from("--body"),
            OsString::from("replace the draft"),
            OsString::from("--delete-last"),
        ];
        assert!(matches!(
            classify(&delete_last, &manifest, "macos"),
            Classification::Unclassified
        ));

        // The edit-last allowance applies only to the explicitly supported issue
        // and pull-request comment tuples; another governed tuple must remain
        // unclassified when it carries this flag.
        let reaction_edit = [
            OsString::from("issue"),
            OsString::from("reaction"),
            OsString::from("42"),
            OsString::from("--reaction"),
            OsString::from("+1"),
            OsString::from("--edit-last"),
        ];
        assert!(matches!(
            classify(&reaction_edit, &manifest, "macos"),
            Classification::Unclassified
        ));
    }

    #[test]
    fn producer_edit_last_vectors_pin_consumer_wire_request_and_refusals() {
        const EXPECTED_SHA256: &str =
            "cd22bb4de80b5c44b500d75220f03d3b0908f0e67101842de0c29c86b1e9b9e0";
        let fixture_bytes = include_bytes!("../tests/fixtures/gh_shim/edit-last-vectors-v1.json");
        assert_eq!(
            format!("{:x}", Sha256::digest(fixture_bytes)),
            EXPECTED_SHA256,
            "producer edit-last vectors changed; re-pin by copying the fixture from repo CortexKit/prefrontal at commit 0b1dea6b, then update this consumer fixture and digest"
        );

        let vectors = edit_last_vectors_fixture();
        let vector_case = |name: &str| {
            vectors["cases"]
                .as_array()
                .expect("producer vector cases")
                .iter()
                .find(|case| case["name"] == name)
                .unwrap_or_else(|| panic!("producer vector case {name} is missing"))
        };
        let happy_request = vector_case("edit_last_happy")["request"].clone();
        let happy_body_fields = happy_request["body"]
            .as_object()
            .expect("producer happy request body")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert!(vector_case("absent_edit_last_create")["request"]
            .get("edit_last")
            .is_none());

        let manifest = v10_fixture_manifest();
        let args = [
            OsString::from("pr"),
            OsString::from("comment"),
            OsString::from("372"),
            OsString::from("--edit-last"),
            OsString::from("--body-file"),
            OsString::from("-"),
        ];
        let Classification::Governed { tuple, canonical } = classify(&args, &manifest, "macos")
        else {
            panic!("the native edit-last command must remain governed");
        };
        assert_eq!(tuple, "pr comment");
        let request = canonicalize_governed(&args, &tuple, &canonical, manifest.manifest_version)
            .expect("native edit-last command should canonicalize");
        let determination =
            RungDetermination::r3(1, manifest.manifest_version, &test_rung_provenance());
        let wire = governed_wire_request(&determination.record, "consumer-agent", request);

        // Compare the complete request shape after replacing values that are
        // intentionally different for this consumer command or process.
        let mut expected = happy_request;
        expected["action"] = json!("pr comment");
        expected["target"] = json!({"number": "372"});
        expected["body"] = wire["body"].clone();
        expected["manifest_version"] = json!(manifest.manifest_version);
        expected["rung_as_of_unix_secs"] = json!(determination.record.as_of_unix_secs);
        expected["repository"] = wire["repository"].clone();
        expected["metadata"]["pid"] = json!(std::process::id());
        expected["metadata"]
            .as_object_mut()
            .expect("expected metadata object")
            .remove("agent_id");
        let mut actual = wire;
        actual["metadata"]
            .as_object_mut()
            .expect("actual metadata object")
            .remove("agent_id");
        assert_eq!(
            actual["body"]
                .as_object()
                .expect("actual request body")
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            happy_body_fields,
            "consumer body fields drifted from producer shape"
        );
        assert_eq!(
            actual, expected,
            "consumer request drifted from producer shape"
        );
        assert_eq!(
            actual["edit_last"], true,
            "edit_last marker must be present"
        );

        for case_name in ["edit_last_no_own_comment", "edit_last_unsupported_action"] {
            let code = vector_case(case_name)["response"]["refusal_code"]
                .as_str()
                .expect("producer refusal code");
            let response = json!({"outcome": "refusal", "refusal_code": code});
            let outcome = parse_governed_response(&serde_json::to_vec(&response).unwrap())
                .expect("producer refusal should parse");
            assert!(matches!(outcome, RouteOutcome::Refusal(ref actual) if actual == code));
            assert_eq!(
                RefusalCode::SeamRefusal.as_str(),
                "gh_shim_seam_refusal",
                "open-world holder refusal codes must use the seam refusal classification"
            );
            assert_eq!(
                seam_refusal_text(code),
                format!("governance seam refused the action: {code}"),
                "holder refusal code must pass through without remapping"
            );
        }
    }

    /// Signed v10 manifests key in-row prose as `reasoning`. This test pins the
    /// exact signed row shape through parse AND a full parse->serialize->parse
    /// round trip, so a field rename can never again silently drop signed
    /// justification text. Mutation control: removing the `reasoning` alias on
    /// `TupleDecl::Details::rationale` must turn this test red by name.
    #[test]
    fn signed_v10_reasoning_prose_survives_parse_and_cache_round_trip() {
        // Byte shape lifted from the signed v10 artifact (admin tier row).
        let signed_row = r#"{
            "tuple": "workflow run",
            "platform": ["macos", "linux"],
            "reasoning": "Administration: dispatching a workflow runs code but carries no public attribution surface; operator identity under explicit bypass."
        }"#;
        let parsed: TupleDecl = serde_json::from_str(signed_row).expect("signed row parses");
        let TupleDecl::Details { rationale, .. } = &parsed else {
            panic!("signed row must parse as a detailed declaration");
        };
        let prose = rationale
            .as_deref()
            .expect("signed `reasoning` prose must survive the parse, not default to None");
        assert!(
            prose.starts_with("Administration:"),
            "prose intact: {prose}"
        );

        // The cache view is a re-serialization of the parsed struct; the prose
        // must survive that full round trip too (this is the view that showed
        // rationale: null for every signed row before the alias existed).
        let cache_bytes = serde_json::to_string(&parsed).expect("cache serialization");
        let reparsed: TupleDecl = serde_json::from_str(&cache_bytes).expect("cache view reparses");
        let TupleDecl::Details {
            rationale: cached, ..
        } = &reparsed
        else {
            panic!("cache view must stay a detailed declaration");
        };
        assert_eq!(
            cached.as_deref(),
            Some(prose),
            "prose must survive the parse->serialize->parse cache round trip verbatim"
        );
    }

    #[test]
    fn manifest_rejects_duplicate_tiers_and_empty_api_rationales() {
        let mut duplicate = fixture_manifest();
        duplicate
            .tiers
            .get_mut(&Tier::Admin)
            .unwrap()
            .push(TupleDecl::Details {
                tuple: "issue comment".to_string(),
                platform: vec!["macos".to_string()],
                api_match: None,
                rationale: None,
            });
        assert!(duplicate.validate().unwrap_err().contains("both"));

        let mut empty_api = fixture_manifest();
        empty_api
            .tiers
            .get_mut(&Tier::Admin)
            .unwrap()
            .push(TupleDecl::Details {
                tuple: "api patch close".to_string(),
                platform: vec!["macos".to_string()],
                api_match: Some(String::new()),
                rationale: None,
            });
        assert!(empty_api.validate().unwrap_err().contains("rationale"));

        let mut malformed_binding = fixture_manifest();
        malformed_binding.bindings.insert(
            "https://github.com/cortexkit/aft.git".to_string(),
            "alfonso-aft".to_string(),
        );
        assert!(malformed_binding
            .validate()
            .unwrap_err()
            .contains("canonical owner/name"));
    }

    #[test]
    fn binding_keys_and_governed_session_identity_are_stable() {
        assert_eq!(
            canonical_repository_key("https://github.com/CortexKit/aft.git"),
            Some("cortexkit/aft".to_string())
        );
        assert_eq!(
            canonical_repository_key("git@github.com:cortexkit/aft.git"),
            Some("cortexkit/aft".to_string())
        );
        assert_eq!(gh_session_id("alfonso-aft"), "gh-shim:alfonso-aft");

        let request = GovernedRequest {
            action: "issue comment".to_string(),
            target: Map::new(),
            body: Map::new(),
            repository: Some("cortexkit/aft".to_string()),
            manifest_version: 1,
            edit_last: false,
        };
        let determination = RungDetermination::r3(7, 1, &test_rung_provenance());
        let wire = governed_wire_request(&determination.record, "alfonso-aft", request);
        assert_eq!(wire["metadata"]["agent_id"], "alfonso-aft");
        assert_eq!(wire["metadata"]["pid"], std::process::id());
    }

    #[test]
    fn manifest_rejects_repo_sections_that_add_or_lower_a_tuple() {
        let mut manifest = fixture_manifest();
        manifest.repository_sections.insert(
            "owner/repo".to_string(),
            RepositorySection {
                tiers: BTreeMap::from([(
                    Tier::Mechanical,
                    vec![TupleDecl::Details {
                        tuple: "issue comment".to_string(),
                        platform: vec!["macos".to_string()],
                        api_match: None,
                        rationale: None,
                    }],
                )]),
                removed_tuples: Vec::new(),
            },
        );
        assert!(manifest.validate().unwrap_err().contains("lowers"));

        manifest.repository_sections.insert(
            "owner/repo".to_string(),
            RepositorySection {
                tiers: BTreeMap::from([(
                    Tier::Admin,
                    vec![TupleDecl::Details {
                        tuple: "workflow dispatch".to_string(),
                        platform: vec!["macos".to_string()],
                        api_match: None,
                        rationale: None,
                    }],
                )]),
                removed_tuples: Vec::new(),
            },
        );
        assert!(manifest.validate().unwrap_err().contains("adds"));
    }

    #[test]
    fn signed_cache_rejects_tampering_and_old_schema_floor() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let now = TEST_NOW;
        write_signed_manifest(&paths, fixture_manifest(), now);
        assert_eq!(load_manifest(&paths, now).unwrap().manifest_version, 1);

        // Tamper with the signed manifest bytes inside the envelope: the
        // signature verifies the distributed bytes, so any edit is fatal.
        let mut value: Value = serde_json::from_slice(&fs::read(&paths.manifest).unwrap()).unwrap();
        let tampered =
            value["manifest_bytes"]
                .as_str()
                .unwrap()
                .replacen("issue view", "issue View", 1);
        value["manifest_bytes"] = Value::String(tampered);
        fs::write(&paths.manifest, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            load_manifest(&paths, now),
            Err(ManifestProblem::Invalid(_))
        ));
        // A validation failure immediately enters the regressed arm, so status
        // names that state first and the artifact fault second.
        assert_eq!(
            cached_manifest_report_at(&paths, now).diagnostics,
            vec![
                SelfReportDiagnostic::ManifestRegressed.as_str(),
                SelfReportDiagnostic::ManifestInvalid.as_str(),
            ]
        );

        let mut below_floor = fixture_manifest();
        below_floor.schema_floor = 0;
        write_signed_manifest(&paths, below_floor, now);
        assert!(matches!(
            load_manifest(&paths, now),
            Err(ManifestProblem::BelowFloor { manifest_floor: 0 })
        ));
    }

    #[test]
    fn no_verb_and_help_invocations_are_mechanical_on_a_governed_manifest() {
        let manifest = fixture_manifest();
        for args in [
            Vec::new(),
            vec![OsString::from("--version")],
            vec![OsString::from("--help")],
            vec![OsString::from("-h")],
            vec![OsString::from("help"), OsString::from("pr")],
        ] {
            assert!(
                matches!(
                    classify(&args, &manifest, "macos"),
                    Classification::Mechanical
                ),
                "expected passthrough classification for {args:?}"
            );
        }
    }

    #[test]
    fn unmapped_get_and_actions_reads_are_mechanical_but_writes_remain_unclassified() {
        let mut manifest = fixture_manifest();
        manifest.api_rules.clear();

        for args in [
            vec![
                OsString::from("api"),
                OsString::from("/repos/cortexkit/aft/actions/runs"),
            ],
            vec![
                OsString::from("api"),
                OsString::from("--method"),
                OsString::from("GET"),
                OsString::from("/repos/cortexkit/aft/actions/runs"),
            ],
            vec![
                OsString::from("api"),
                OsString::from("-X"),
                OsString::from("GET"),
                OsString::from("/repos/cortexkit/aft/actions/runs"),
            ],
            vec![OsString::from("run"), OsString::from("view")],
            vec![OsString::from("run"), OsString::from("list")],
            vec![OsString::from("run"), OsString::from("watch")],
            vec![OsString::from("workflow"), OsString::from("view")],
            vec![OsString::from("workflow"), OsString::from("list")],
        ] {
            assert!(
                matches!(
                    classify(&args, &manifest, "macos"),
                    Classification::Mechanical
                ),
                "expected read passthrough classification for {args:?}"
            );
        }

        for args in [
            vec![
                OsString::from("api"),
                OsString::from("-X"),
                OsString::from("POST"),
                OsString::from("/repos/cortexkit/aft/actions/runs"),
            ],
            vec![
                OsString::from("api"),
                OsString::from("-f"),
                OsString::from("key=value"),
                OsString::from("/repos/cortexkit/aft/actions/runs"),
            ],
        ] {
            assert!(
                matches!(
                    classify(&args, &manifest, "macos"),
                    Classification::Unclassified
                ),
                "expected fail-closed classification for {args:?}"
            );
        }
    }

    #[test]
    fn classification_is_allowlist_driven_without_a_write_heuristic() {
        let manifest = fixture_manifest();
        assert!(matches!(
            classify(
                &[OsString::from("issue"), OsString::from("view")],
                &manifest,
                "macos"
            ),
            Classification::Mechanical
        ));
        assert!(matches!(
            classify(
                &[OsString::from("api"), OsString::from("/repos/a/b")],
                &manifest,
                "macos"
            ),
            Classification::Mechanical
        ));
        assert!(matches!(
            classify(
                &[
                    OsString::from("api"),
                    OsString::from("--method=POST"),
                    OsString::from("/repos/a/b")
                ],
                &manifest,
                "macos"
            ),
            Classification::Unclassified
        ));
        assert!(matches!(
            classify(
                &[
                    OsString::from("api"),
                    OsString::from("--method"),
                    OsString::from("POST"),
                    OsString::from("/repos/a/b")
                ],
                &manifest,
                "macos"
            ),
            Classification::Unclassified
        ));
        assert!(matches!(
            classify(
                &[OsString::from("alias"), OsString::from("set")],
                &manifest,
                "macos"
            ),
            Classification::Unclassified
        ));
        assert!(matches!(
            classify(
                &[
                    OsString::from("alias"),
                    OsString::from("set"),
                    OsString::from("--write")
                ],
                &manifest,
                "macos"
            ),
            Classification::Unclassified
        ));
    }

    #[test]
    fn canonical_repository_key_parses_github_remotes_and_rejects_foreign_hosts() {
        for remote in [
            "https://github.com/CortexKit/Aft",
            "https://github.com/cortexkit/aft.git",
            "https://github.com/cortexkit/aft/",
            "https://github.com/cortexkit/aft.git/",
            "git@github.com:cortexkit/aft.git",
            "ssh://git@github.com/cortexkit/aft",
            "cortexkit/aft",
        ] {
            assert_eq!(
                canonical_repository_key(remote).as_deref(),
                Some("cortexkit/aft")
            );
        }
        for remote in [
            "https://gitlab.com/cortexkit/aft.git",
            "ssh://git@gitlab.com/cortexkit/aft",
            "git@gitlab.com:cortexkit/aft.git",
        ] {
            assert_eq!(canonical_repository_key(remote), None);
        }
    }

    #[test]
    fn invalid_repository_argument_refuses_before_seam_routing() {
        let manifest = fixture_manifest();
        let canonical = manifest.canonicalization["issue comment"].clone();
        let error = canonicalize_governed(
            &[
                OsString::from("--repo"),
                OsString::from("not/an/owner-name"),
                OsString::from("issue"),
                OsString::from("comment"),
                OsString::from("42"),
                OsString::from("--body"),
                OsString::from("hello"),
            ],
            "issue comment",
            &canonical,
            1,
        )
        .expect_err("an unparseable repository must abort before seam routing");
        assert_eq!(error, "repository not/an/owner-name is not owner/name");
        assert_eq!(
            refuse_governed_canonicalization(&error),
            REFUSAL_EXIT_STATUS,
            "a pre-routing governance refusal must have a nonzero exit status"
        );
    }

    #[test]
    fn governed_canonicalization_normalizes_flags_and_explicit_repo_wins() {
        let manifest = fixture_manifest();
        let canonical = manifest.canonicalization["issue comment"].clone();
        let request = canonicalize_governed(
            &[
                OsString::from("--repo=owner/explicit"),
                OsString::from("issue"),
                OsString::from("comment"),
                OsString::from("42"),
                OsString::from("--body"),
                OsString::from("hello"),
            ],
            "issue comment",
            &canonical,
            1,
        )
        .unwrap();
        assert_eq!(request.repository.as_deref(), Some("owner/explicit"));
        assert_eq!(request.target["number"], "42");
        assert_eq!(request.body["body"], "hello");
    }

    #[test]
    fn speech_body_file_forms_are_allowed_and_forward_fixture_contents() {
        let manifest = fixture_manifest();
        let body_file = fixture_dir().join("governed-speech.md");
        let expected_body = fs::read_to_string(&body_file).expect("speech body fixture");

        for (expected_tuple, verb, subcommand, target) in [
            ("issue comment", "issue", "comment", "42"),
            ("pr comment", "pr", "comment", "7"),
            ("pr review", "pr", "review", "7"),
        ] {
            let canonical = manifest.canonicalization[expected_tuple].clone();
            for (flag, suffix) in [("--body-file", ""), ("-F", "")]
                .into_iter()
                .chain([("--body-file=", "equals"), ("-F=", "equals")])
            {
                let file_arg = if suffix.is_empty() {
                    body_file.to_string_lossy().into_owned()
                } else {
                    format!("{flag}{}", body_file.display())
                };
                let args = if suffix.is_empty() {
                    vec![
                        OsString::from(verb),
                        OsString::from(subcommand),
                        OsString::from(target),
                        OsString::from(flag),
                        OsString::from(file_arg),
                    ]
                } else {
                    vec![
                        OsString::from(verb),
                        OsString::from(subcommand),
                        OsString::from(target),
                        OsString::from(file_arg),
                    ]
                };
                assert!(matches!(
                    classify(&args, &manifest, "macos"),
                    Classification::Governed { ref tuple, .. } if tuple == expected_tuple
                ));
                let request = canonicalize_governed(&args, expected_tuple, &canonical, 1)
                    .expect("body-file form should canonicalize");
                let determination = RungDetermination::r3(1, 1, &test_rung_provenance());
                let wire = governed_wire_request(&determination.record, "agent-7", request);
                assert_eq!(wire["body"]["body"], expected_body);
            }
        }

        let reaction = manifest.canonicalization["issue reaction"].clone();
        let error = canonicalize_governed(
            &[
                OsString::from("issue"),
                OsString::from("reaction"),
                OsString::from("42"),
                OsString::from("--body-file"),
                OsString::from(body_file),
            ],
            "issue reaction",
            &reaction,
            1,
        )
        .expect_err("body-file is speech-only vocabulary");
        assert_eq!(error, "undeclared flag --body-file");
    }

    #[test]
    fn body_file_failures_refuse_instead_of_forwarding_an_empty_body() {
        let manifest = fixture_manifest();
        let canonical = manifest.canonicalization["pr comment"].clone();
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.md");
        let invalid = directory.path().join("invalid-utf8.md");
        fs::write(&invalid, [0xff, 0xfe]).unwrap();

        for path in [missing, invalid] {
            let error = canonicalize_governed(
                &[
                    OsString::from("pr"),
                    OsString::from("comment"),
                    OsString::from("7"),
                    OsString::from("--body-file"),
                    OsString::from(&path),
                ],
                "pr comment",
                &canonical,
                1,
            )
            .expect_err("an unreadable body file must refuse");
            assert!(error.starts_with("--body-file: could not read body file "));
            assert!(error.contains(&path.display().to_string()));
            assert_eq!(
                refuse_governed_canonicalization(&error),
                REFUSAL_EXIT_STATUS
            );
        }
    }

    #[test]
    fn body_file_dash_reads_stdin_under_the_caller_permissions() {
        let mut stdin = std::io::Cursor::new("body supplied through stdin");
        assert_eq!(
            read_body_file_from(Path::new("-"), &mut stdin).unwrap(),
            "body supplied through stdin"
        );
    }

    #[test]
    fn pr_review_action_and_body_matrix_reaches_the_governed_payload() {
        let manifest = fixture_manifest();
        let canonical = manifest.canonicalization["pr review"].clone();
        let body_file = fixture_dir().join("governed-speech.md");
        let expected_body = fs::read_to_string(&body_file).expect("speech body fixture");

        for (action_flag, event) in [
            ("--approve", "APPROVE"),
            ("--comment", "COMMENT"),
            ("--request-changes", "REQUEST_CHANGES"),
        ] {
            for (body_flag, body_value) in [("--body", "inline review"), ("-b", "short review")] {
                let args = vec![
                    OsString::from("pr"),
                    OsString::from("review"),
                    OsString::from("7"),
                    OsString::from(action_flag),
                    OsString::from(body_flag),
                    OsString::from(body_value),
                ];
                assert!(matches!(
                    classify(&args, &manifest, "macos"),
                    Classification::Governed { ref tuple, .. } if tuple == "pr review"
                ));
                let request = canonicalize_governed(&args, "pr review", &canonical, 1)
                    .expect("review action with inline body should canonicalize");
                assert_eq!(request.body["event"], event);
                assert_eq!(request.body["body"], body_value);
            }

            let args = vec![
                OsString::from("pr"),
                OsString::from("review"),
                OsString::from("7"),
                OsString::from(action_flag),
                OsString::from("--body-file"),
                OsString::from(&body_file),
            ];
            let request = canonicalize_governed(&args, "pr review", &canonical, 1)
                .expect("review action with body-file should canonicalize");
            assert_eq!(request.body["event"], event);
            assert_eq!(request.body["body"], expected_body);
        }

        for action_flag in ["--approve", "--request-changes"] {
            let args = vec![
                OsString::from("pr"),
                OsString::from("review"),
                OsString::from("7"),
                OsString::from(action_flag),
            ];
            let request = canonicalize_governed(&args, "pr review", &canonical, 1)
                .expect("approve/request-changes may omit review prose");
            assert_eq!(
                request.body["event"],
                action_flag
                    .trim_start_matches("--")
                    .to_ascii_uppercase()
                    .replace('-', "_")
            );
            assert!(!request.body.contains_key("body"));
        }

        let duplicate = [
            OsString::from("pr"),
            OsString::from("review"),
            OsString::from("7"),
            OsString::from("--approve"),
            OsString::from("--comment"),
            OsString::from("--body"),
            OsString::from("review"),
        ];
        assert_eq!(
            canonicalize_governed(&duplicate, "pr review", &canonical, 1).unwrap_err(),
            "pr review accepts only one of --approve, --comment, or --request-changes"
        );
    }

    #[test]
    fn upstream_api_errors_fail_without_changing_success_status() {
        let error_response = json!({
            "outcome": "result",
            "gh_route_schema": 1,
            "result": {
                "status": 404,
                "error": {"message": "Not Found", "documentation_url": "https://docs.github.com"}
            }
        });
        let error_outcome =
            parse_governed_response(&serde_json::to_vec(&error_response).unwrap()).unwrap();
        let error_body = match error_outcome {
            RouteOutcome::UpstreamError(body) => body,
            other => panic!("expected upstream error, got {other:?}"),
        };
        assert!(error_body.contains("Not Found"));
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let binding = AgentBinding {
            repo: "owner/repo".to_string(),
            agent_id: "agent-7".to_string(),
        };
        assert_eq!(
            governed_outcome_status(
                &paths,
                &binding,
                123,
                RouteOutcome::UpstreamError(error_body)
            ),
            UPSTREAM_FAILURE_EXIT_STATUS
        );

        let success_response = json!({
            "outcome": "result",
            "gh_route_schema": 1,
            "result": {"status": 201, "url": "https://github.com/example"},
            "field_order": ["status", "url"]
        });
        let success_outcome =
            parse_governed_response(&serde_json::to_vec(&success_response).unwrap()).unwrap();
        assert!(matches!(&success_outcome, RouteOutcome::Result(_)));
        assert_eq!(
            governed_outcome_status(&paths, &binding, 123, success_outcome),
            0
        );
    }

    #[test]
    fn governed_renderer_is_deterministic_for_scalars_arrays_and_escapes() {
        let result = json!({"message":"snowman ☃\n", "items":["a", 2], "ok":true});
        let order = vec![json!("ok"), json!("message"), json!("items")];
        assert_eq!(
            render_governed_response(&result, &order).unwrap(),
            "ok: true\nmessage: \"snowman ☃\\n\"\nitems:\n  \"a\"\n  2\n"
        );
        assert!(matches!(
            render_governed_response(&json!("scalar"), &order),
            Err(RouteOutcome::SchemaMismatch(_))
        ));
    }

    #[test]
    fn lower_rungs_are_cached_durably_but_r1_is_not_written() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let determination = RungDetermination::r2(
            123,
            R2Reason::DaemonUnreachable,
            None,
            &test_rung_provenance(),
        );
        write_rung_record_silently(&paths, &determination.record);
        assert_eq!(load_rung_record(&paths).unwrap().rung, Rung::R2);
        assert!(!paths.root.join("r1-cache.json").exists());
    }

    #[test]
    fn governed_bound_disposition_is_reason_independent_except_operator_hard_off() {
        const EXPECTED_RUNG_SHAPE_COUNT: usize = 11;

        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let connection_file = directory.path().join("connection.json");
        fs::write(&connection_file, "present").unwrap();
        let missing_connection = directory.path().join("missing-connection.json");
        let disabled_doc = serde_json::json!({
            "gh_shim": { "enabled": false },
            "subc": { "connection_file": missing_connection }
        })
        .to_string();
        let unreachable_doc = serde_json::json!({
            "subc": { "connection_file": directory.path().join("still-missing.json") }
        })
        .to_string();
        let budget_doc = serde_json::json!({
            "subc": { "connection_file": connection_file }
        })
        .to_string();
        let future_deadline = || std::time::Instant::now() + DISCOVERY_BUDGET;
        let r1_cases = [
            (
                R1Reason::DisabledByConfig,
                determine_rung_from_doc(
                    &paths,
                    directory.path(),
                    1,
                    future_deadline(),
                    Some(&disabled_doc),
                ),
            ),
            (
                R1Reason::AbsentOrUnparseable,
                determine_rung_from_doc(&paths, directory.path(), 1, future_deadline(), Some("{}")),
            ),
            (
                R1Reason::Unreachable,
                determine_rung_from_doc(
                    &paths,
                    directory.path(),
                    1,
                    future_deadline(),
                    Some(&unreachable_doc),
                ),
            ),
            (
                R1Reason::DiscoveryBudgetExhausted,
                determine_rung_from_doc(
                    &paths,
                    directory.path(),
                    1,
                    std::time::Instant::now() - Duration::from_millis(1),
                    Some(&budget_doc),
                ),
            ),
        ];
        assert_eq!(r1_cases.len(), R1Reason::ALL.len());
        for (reason, determination) in &r1_cases {
            assert_eq!(determination.record.rung, Rung::R1);
            assert_eq!(
                determination
                    .record
                    .inputs
                    .get("connection_file")
                    .map(String::as_str),
                Some(reason.diagnostic())
            );
        }

        let mut determinations = r1_cases
            .into_iter()
            .map(|(_, determination)| determination)
            .collect::<Vec<_>>();
        determinations.extend(
            R2Reason::ALL
                .into_iter()
                .map(|reason| RungDetermination::r2(1, reason, Some(1), &test_rung_provenance())),
        );
        determinations.push(RungDetermination::r3(1, 1, &test_rung_provenance()));
        assert_eq!(
            R1Reason::ALL.len() + R2Reason::ALL.len() + 1,
            EXPECTED_RUNG_SHAPE_COUNT,
            "update the explicit disposition matrix when a rung shape is added"
        );
        assert_eq!(determinations.len(), EXPECTED_RUNG_SHAPE_COUNT);

        let manifest = fixture_manifest();
        let governed_args = [
            OsString::from("issue"),
            OsString::from("comment"),
            OsString::from("42"),
            OsString::from("--body"),
            OsString::from("hello"),
        ];
        let admin_args = [
            OsString::from("pr"),
            OsString::from("merge"),
            OsString::from("42"),
        ];
        let mechanical_args = [
            OsString::from("issue"),
            OsString::from("view"),
            OsString::from("42"),
        ];
        let governed = classify(&governed_args, &manifest, "macos");
        let admin = classify(&admin_args, &manifest, "macos");
        let mechanical = classify(&mechanical_args, &manifest, "macos");
        let binding = || AgentBinding {
            repo: "cortexkit/aft".to_string(),
            agent_id: "alfonso-aft".to_string(),
        };

        for determination in &determinations {
            let bound_governed = structural_governance_disposition(
                determination,
                &governed,
                Some(binding()),
                manifest.manifest_version,
            );
            if determination.operator_disabled {
                assert!(matches!(bound_governed, GovernanceDisposition::Delegate));
            } else if determination.record.rung == Rung::R3 {
                assert!(matches!(bound_governed, GovernanceDisposition::Ready));
            } else {
                assert!(matches!(
                    bound_governed,
                    GovernanceDisposition::Unavailable(_)
                ));
            }

            assert!(matches!(
                structural_governance_disposition(
                    determination,
                    &governed,
                    None,
                    manifest.manifest_version,
                ),
                GovernanceDisposition::Delegate
            ));
            assert!(matches!(
                structural_governance_disposition(
                    determination,
                    &mechanical,
                    Some(binding()),
                    manifest.manifest_version,
                ),
                GovernanceDisposition::Delegate
            ));

            if determination.record.rung != Rung::R3 && !determination.operator_disabled {
                assert!(matches!(
                    structural_governance_disposition(
                        determination,
                        &admin,
                        Some(binding()),
                        manifest.manifest_version,
                    ),
                    GovernanceDisposition::Unavailable(_)
                ));
            }
        }
    }

    #[test]
    fn ambient_credentials_on_a_bound_governed_invocation_refuse_identity_ambiguity() {
        let manifest = fixture_manifest();
        let governed = classify(
            &[
                OsString::from("issue"),
                OsString::from("comment"),
                OsString::from("42"),
                OsString::from("--body"),
                OsString::from("hello"),
            ],
            &manifest,
            "macos",
        );
        let determination = RungDetermination::r2(
            1,
            R2Reason::AgentCredentialsPresent,
            Some(manifest.manifest_version),
            &test_rung_provenance(),
        );
        let binding = AgentBinding {
            repo: "cortexkit/aft".to_string(),
            agent_id: "alfonso-aft".to_string(),
        };

        assert!(matches!(
            structural_governance_disposition(
                &determination,
                &governed,
                Some(binding),
                manifest.manifest_version,
            ),
            GovernanceDisposition::Unavailable(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn resolved_image_identity_skips_a_shim_reached_through_a_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("aft");
        fs::write(&image, b"shim image").unwrap();
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).unwrap();
        symlink(&image, bin.join("gh")).unwrap();
        let linked_parent = directory.path().join("linked-bin");
        symlink(&bin, &linked_parent).unwrap();

        assert!(same_image(&linked_parent.join("gh"), &image));
    }

    #[test]
    fn bypass_audit_is_visible_to_a_later_self_report_reader() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        append_bypass_audit(&paths, "issue close", Some("owner/repo"), 99).unwrap();
        let (records, error) = read_bypass_audit(&paths);
        assert!(error.is_none());
        let records = records.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["tuple"], "issue close");
    }

    #[test]
    fn refusal_and_self_report_codes_are_separate_closed_sets() {
        assert_eq!(RefusalCode::ALL.len(), 11);
        assert!(RefusalCode::ALL
            .iter()
            .all(|code| code.as_str().starts_with("gh_shim_")));
        assert_eq!(
            RefusalCode::GovernanceUnavailable.as_str(),
            "gh_shim_governance_unavailable"
        );
        assert_eq!(
            GOVERNANCE_UNAVAILABLE_TEXT,
            "the governance daemon is unreachable and this repository's actions are identity-governed; retry after the daemon returns"
        );
        assert_eq!(SelfReportDiagnostic::ALL.len(), 6);
        assert!(SelfReportDiagnostic::ALL
            .iter()
            .all(|code| code.as_str().starts_with("gh_shim_status_")));
        assert!(SelfReportDiagnostic::ALL
            .iter()
            .all(|code| !code.as_str().contains("stale")));
        assert_eq!(REFUSAL_EXIT_STATUS, 86);
    }

    #[test]
    fn v1_write_classification_accepts_only_the_reviewed_tuple_sets() {
        let manifest = fixture_manifest();
        for tuple in V1_GOVERNED_TUPLES {
            let args = tuple
                .split_whitespace()
                .map(OsString::from)
                .collect::<Vec<_>>();
            assert!(matches!(
                classify(&args, &manifest, "macos"),
                Classification::Governed { .. }
            ));
        }
        for tuple in V1_ADMIN_TUPLES {
            let args = tuple
                .split_whitespace()
                .map(OsString::from)
                .collect::<Vec<_>>();
            assert!(matches!(
                classify(&args, &manifest, "macos"),
                Classification::Admin { .. }
            ));
        }
        for args in [
            ["release", "publish"].as_slice(),
            ["issue", "create"].as_slice(),
            ["pr", "reopen"].as_slice(),
        ] {
            let args = args.iter().map(OsString::from).collect::<Vec<_>>();
            assert!(matches!(
                classify(&args, &manifest, "macos"),
                Classification::Unclassified
            ));
        }
    }

    #[test]
    fn field_bearing_api_forms_remain_unclassified_without_an_audited_parser() {
        let manifest = fixture_manifest();
        for field_flag in [
            "--field=name=value",
            "--raw-field=name=value",
            "--input=body.json",
            "-fname=value",
            "-Fname=value",
        ] {
            let args = vec![
                OsString::from("api"),
                OsString::from("/repos/owner/repo"),
                OsString::from(field_flag),
            ];
            assert!(matches!(
                classify(&args, &manifest, "macos"),
                Classification::Unclassified
            ));
        }
    }

    #[test]
    fn holder_refusals_preserve_any_string_code_and_reject_non_strings() {
        for code in FIXTURE_ACCEPTED_SEAM_REFUSAL_CODES {
            let response = json!({"outcome": "refusal", "refusal_code": code});
            let outcome = parse_governed_response(&serde_json::to_vec(&response).unwrap()).unwrap();
            assert!(matches!(outcome, RouteOutcome::Refusal(ref actual) if actual == code));
            assert_eq!(RefusalCode::SeamRefusal.as_str(), "gh_shim_seam_refusal");
            assert_eq!(
                seam_refusal_text(code),
                format!("governance seam refused the action: {code}")
            );
            assert_eq!(REFUSAL_EXIT_STATUS, 86);
        }
        let unknown = "quota_exhausted_v2";
        let response = json!({"outcome": "refusal", "refusal_code": unknown});
        let outcome = parse_governed_response(&serde_json::to_vec(&response).unwrap()).unwrap();
        assert!(matches!(outcome, RouteOutcome::Refusal(ref actual) if actual == unknown));
        assert_eq!(RefusalCode::SeamRefusal.as_str(), "gh_shim_seam_refusal");
        assert_eq!(
            seam_refusal_text(unknown),
            "governance seam refused the action: quota_exhausted_v2"
        );
        assert_eq!(REFUSAL_EXIT_STATUS, 86);

        for response in [
            json!({"outcome": "refusal", "refusal_code": 7}),
            json!({"outcome": "refusal", "refusal_code": null}),
            json!({"outcome": "refusal"}),
        ] {
            assert!(matches!(
                parse_governed_response(&serde_json::to_vec(&response).unwrap()),
                Err(RouteOutcome::SchemaMismatch(_))
            ));
        }
    }

    #[test]
    fn governed_self_report_transitions_are_durable_and_mechanical_classification_preserves_them() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let binding = AgentBinding {
            repo: "owner/repo".to_string(),
            agent_id: "agent-7".to_string(),
        };
        write_seam_state(
            &paths,
            SeamState {
                bound_holder: None,
                agent_binding: Some(binding.clone()),
                last_seam_refusal: None,
            },
        )
        .unwrap();
        let report = build_self_report(&paths);
        assert_eq!(report.bound_holder, None);
        assert_eq!(report.agent_binding, Some(binding.clone()));
        assert_eq!(report.last_seam_refusal, None);

        write_seam_state(
            &paths,
            SeamState {
                bound_holder: Some(ROUTING_HOLDER_MODULE_ID.to_string()),
                agent_binding: Some(binding.clone()),
                last_seam_refusal: Some(LastSeamRefusal {
                    code: "rate_limited".to_string(),
                    at_unix_secs: 77,
                }),
            },
        )
        .unwrap();
        let report = build_self_report(&paths);
        assert_eq!(
            report.bound_holder.as_deref(),
            Some(ROUTING_HOLDER_MODULE_ID)
        );
        assert_eq!(report.agent_binding, Some(binding.clone()));
        assert_eq!(
            report
                .last_seam_refusal
                .as_ref()
                .map(|refusal| refusal.code.as_str()),
            Some("rate_limited")
        );

        write_seam_state(
            &paths,
            governed_seam_state(&paths, Some(ROUTING_HOLDER_MODULE_ID.to_string()), &binding),
        )
        .unwrap();
        assert_eq!(
            seam_state(&paths)
                .last_seam_refusal
                .as_ref()
                .map(|refusal| refusal.code.as_str()),
            Some("rate_limited")
        );

        let mechanical = [OsString::from("issue"), OsString::from("view")];
        assert!(matches!(
            classify(&mechanical, &fixture_manifest(), "macos"),
            Classification::Mechanical
        ));
        assert_eq!(
            seam_state(&paths)
                .last_seam_refusal
                .as_ref()
                .map(|refusal| refusal.at_unix_secs),
            Some(77)
        );
    }

    #[test]
    fn governed_self_report_persistence_failure_is_loud() {
        let directory = tempfile::tempdir().unwrap();
        let state_root = directory.path().join("not-a-directory");
        fs::write(&state_root, b"file").unwrap();
        let paths = StatePaths::from_root(state_root);
        assert!(write_seam_state(&paths, SeamState::default()).is_err());
    }

    #[test]
    fn raw_bytes_round_trip_verifies_then_parses_from_the_fixture_envelope() {
        let envelope: SignedManifest = serde_json::from_str(include_str!(
            "../tests/fixtures/gh_shim/signed-envelope-v2.json"
        ))
        .expect("signed envelope fixture");
        // The embedded bytes are exactly the published manifest file.
        assert_eq!(
            envelope.manifest_bytes,
            include_str!("../tests/fixtures/gh_shim/initial-manifest-v1.json")
        );
        // Verify the received bytes first, parse second.
        let manifest = verify_manifest_signature(&envelope).expect("fixture signature verifies");
        assert_eq!(manifest.manifest_version, 1);
        assert_eq!(manifest.issued_at_unix_secs, FIXTURE_ISSUED_AT);
        manifest.validate().expect("fixture manifest validates");
    }

    #[test]
    fn tampered_single_byte_fixture_fails_signature_verification() {
        let canonical: SignedManifest = serde_json::from_str(include_str!(
            "../tests/fixtures/gh_shim/signed-envelope-v2.json"
        ))
        .expect("canonical envelope fixture");
        let tampered: SignedManifest = serde_json::from_str(include_str!(
            "../tests/fixtures/gh_shim/signed-envelope-v2-tampered.json"
        ))
        .expect("tampered envelope fixture");
        // The tampering is exactly one substituted byte inside the signed
        // bytes; the signature is untouched.
        assert_eq!(
            canonical.manifest_bytes.len(),
            tampered.manifest_bytes.len()
        );
        assert_eq!(
            canonical
                .manifest_bytes
                .bytes()
                .zip(tampered.manifest_bytes.bytes())
                .filter(|(left, right)| left != right)
                .count(),
            1
        );
        assert_eq!(canonical.signature, tampered.signature);
        assert!(matches!(
            verify_manifest_signature(&tampered),
            Err(ManifestProblem::Invalid(_))
        ));
    }

    #[test]
    fn future_issued_at_fixture_is_refused_and_aged_fixture_serves_governed_classification() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());

        write_envelope_fixture(
            &paths,
            include_str!("../tests/fixtures/gh_shim/signed-envelope-v2-future-issued-at.json"),
        );
        match load_manifest(&paths, TEST_NOW) {
            Err(ManifestProblem::Invalid(error)) => {
                assert!(error.contains("future"), "unexpected error: {error}")
            }
            other => panic!("expected future issued_at refusal, got {other:?}"),
        }

        // This signature is valid, but its provenance timestamp is 2,000,000
        // seconds old. A ceremony-once manifest remains active, so it still
        // classifies governed commands instead of scheduling an outage.
        write_envelope_fixture(
            &paths,
            include_str!("../tests/fixtures/gh_shim/signed-envelope-v2-stale-issued-at.json"),
        );
        let ManifestResolution::Active(manifest) = resolve_manifest(&paths, TEST_NOW) else {
            panic!("expected the aged signed manifest to remain active");
        };
        assert_eq!(manifest.issued_at_unix_secs, FIXTURE_ISSUED_AT - 2_000_000);
        assert!(matches!(
            classify(
                &[
                    OsString::from("issue"),
                    OsString::from("comment"),
                    OsString::from("42"),
                    OsString::from("--body"),
                    OsString::from("hello"),
                ],
                &manifest,
                "macos"
            ),
            Classification::Governed { tuple, .. } if tuple == "issue comment"
        ));

        let report: Value =
            serde_json::from_str(&render_self_report(&paths).expect("self report serialization"))
                .expect("self report JSON");
        assert_eq!(
            report["cached_manifest"]["issued_at_unix_secs"],
            FIXTURE_ISSUED_AT - 2_000_000
        );
    }

    #[test]
    fn standby_key_fixture_verifies_under_a_two_slot_trust_set_and_unknown_key_ids_are_refused() {
        let envelope: SignedManifest = serde_json::from_str(include_str!(
            "../tests/fixtures/gh_shim/signed-envelope-v2-standby-key.json"
        ))
        .expect("standby envelope fixture");

        let standby = Ed25519KeyPair::from_seed_unchecked(&STANDBY_TEST_SEED).expect("standby key");
        assert_ne!(standby.public_key().as_ref(), DEV_MANIFEST_PUBLIC_KEY);
        let standby_public: &'static [u8] =
            Box::leak(standby.public_key().as_ref().to_vec().into_boxed_slice());
        let trust_set = [
            Some(ManifestTrustKey {
                key_id: DEV_MANIFEST_KEY_ID,
                public_key: &DEV_MANIFEST_PUBLIC_KEY,
            }),
            Some(ManifestTrustKey {
                key_id: DEV_STANDBY_MANIFEST_KEY_ID,
                public_key: standby_public,
            }),
        ];

        // A standby-signed manifest is accepted under the two-slot set.
        let manifest =
            verify_manifest_signature_with(&envelope, &trust_set).expect("standby slot verifies");
        assert_eq!(
            manifest.manifest_version,
            fixture_manifest().manifest_version
        );

        // A third, unknown key id is refused by the same set.
        let mut unknown = envelope.clone();
        unknown.key_id = "gh-routing-unknown-key".to_string();
        assert!(matches!(
            verify_manifest_signature_with(&unknown, &trust_set),
            Err(ManifestProblem::Invalid(_))
        ));
    }

    #[test]
    fn compiled_trust_set_shape_matches_the_two_slot_design() {
        let slots = compiled_manifest_trust_set();
        // Every profile trusts the production root minted in the 2026-08-27
        // CKCRED ceremony (`signing:gh-manifest-root:1`); the bytes here are
        // the published public half, re-asserted so a trust-slot edit cannot
        // silently swap the live key.
        let live = slots[0].expect("live slot carries the production root");
        assert_eq!(live.key_id, PROD_MANIFEST_KEY_ID);
        assert_eq!(live.public_key, &PROD_MANIFEST_PUBLIC_KEY);
        #[cfg(debug_assertions)]
        {
            // Debug images verify both eras: prod live + the dev test key so
            // fixtures exercise R3 without a custody round-trip.
            assert_eq!(slots.len(), 2);
            assert_eq!(slots[1].unwrap().key_id, DEV_MANIFEST_KEY_ID);
        }
        #[cfg(not(debug_assertions))]
        {
            // The release set keeps two slots: prod live + a cold standby that
            // stays empty until a future custody release fills it.
            assert_eq!(slots.len(), 2);
            assert!(slots[1].is_none());
        }
    }

    #[test]
    fn envelope_v1_shapes_are_refused_by_the_v2_verifier() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let manifest = fixture_manifest();
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let key = Ed25519KeyPair::from_seed_unchecked(&TEST_SEED).unwrap();
        let signature = base64::engine::general_purpose::STANDARD.encode(key.sign(&bytes).as_ref());

        // The pre-v2 shape carried the parsed manifest object in the envelope.
        let v1_object = json!({
            "artifact_id": MANIFEST_ARTIFACT_ID,
            "key_id": DEV_MANIFEST_KEY_ID,
            "fetched_at_unix_secs": TEST_NOW,
            "signature": signature,
            "manifest": serde_json::to_value(&manifest).unwrap(),
        });
        fs::write(&paths.manifest, serde_json::to_vec(&v1_object).unwrap()).unwrap();
        assert!(matches!(
            load_manifest(&paths, TEST_NOW),
            Err(ManifestProblem::Invalid(_))
        ));

        // An envelope naming an older version is refused even with raw bytes.
        let mut old_version = signed(&manifest, TEST_NOW);
        old_version.envelope_version = 1;
        fs::write(&paths.manifest, serde_json::to_vec(&old_version).unwrap()).unwrap();
        match load_manifest(&paths, TEST_NOW) {
            Err(ManifestProblem::Invalid(error)) => {
                assert!(
                    error.contains("envelope version"),
                    "unexpected error: {error}"
                )
            }
            other => panic!("expected envelope version refusal, got {other:?}"),
        }
    }

    #[test]
    fn dormant_resolution_is_presence_based() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        // No artifact on disk: dormant.
        assert!(matches!(
            resolve_manifest(&paths, TEST_NOW),
            ManifestResolution::Dormant
        ));

        // A failing artifact with no last-valid cache falls back without a
        // regressed classification, but remains distinguishable from a missing
        // public-install manifest so the invocation can announce the fallback.
        let untrusted = signed_with(
            &fixture_manifest(),
            TEST_NOW,
            &STANDBY_TEST_SEED,
            "gh-routing-unknown-key",
        );
        fs::write(&paths.manifest, serde_json::to_vec(&untrusted).unwrap()).unwrap();
        assert!(matches!(
            resolve_manifest(&paths, TEST_NOW),
            ManifestResolution::Invalid(ManifestProblem::Invalid(_))
        ));
    }

    #[test]
    fn regressed_invalid_artifact_refuses_governed_and_admin_and_passes_mechanical() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let now = TEST_NOW;

        // Accept the canonical manifest; this writes the last-valid cache.
        write_signed_manifest(&paths, fixture_manifest(), now);
        load_manifest(&paths, now).expect("canonical manifest verifies");

        // Break the installed artifact: signed bytes tampered after signing.
        write_envelope_fixture(
            &paths,
            include_str!("../tests/fixtures/gh_shim/signed-envelope-v2-tampered.json"),
        );

        // A failed validation immediately enters the regressed arm; time passing
        // does not participate in manifest validity.
        let ManifestResolution::Regressed { manifest, problem } = resolve_manifest(&paths, now)
        else {
            panic!("expected the regressed arm");
        };
        let governed = [
            OsString::from("issue"),
            OsString::from("comment"),
            OsString::from("42"),
            OsString::from("--body"),
            OsString::from("hello"),
        ];
        assert!(matches!(
            regressed_disposition(&governed, &manifest, "macos", &problem),
            RegressedDisposition::Refuse {
                code: RefusalCode::ManifestRegressed,
                ..
            }
        ));
        let admin = [
            OsString::from("pr"),
            OsString::from("merge"),
            OsString::from("1"),
        ];
        assert!(matches!(
            regressed_disposition(&admin, &manifest, "macos", &problem),
            RegressedDisposition::Refuse {
                code: RefusalCode::ManifestRegressed,
                ..
            }
        ));
        let mechanical = [OsString::from("issue"), OsString::from("view")];
        assert!(matches!(
            regressed_disposition(&mechanical, &manifest, "macos", &problem),
            RegressedDisposition::Passthrough
        ));
        let undeclared = [OsString::from("alias"), OsString::from("set")];
        assert!(matches!(
            regressed_disposition(&undeclared, &manifest, "macos", &problem),
            RegressedDisposition::Refuse {
                code: RefusalCode::Unclassified,
                ..
            }
        ));

        // The self report is loud about the regressed validation failure.
        let report = cached_manifest_report_at(&paths, now);
        assert_eq!(report.state, Some("regressed"));
        assert_eq!(report.version, Some(1));
        assert_eq!(report.issued_at_unix_secs, Some(FIXTURE_ISSUED_AT));
        assert_eq!(
            report.diagnostics,
            vec![
                SelfReportDiagnostic::ManifestRegressed.as_str(),
                SelfReportDiagnostic::ManifestInvalid.as_str(),
            ]
        );
    }

    #[test]
    fn self_report_exposes_manifest_and_rung_record_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        write_signed_manifest(&paths, fixture_manifest(), TEST_NOW);

        let manifest_report = cached_manifest_report_at(&paths, TEST_NOW);
        assert_eq!(manifest_report.state, Some("valid"));
        assert_eq!(
            manifest_report.verified_by_key_id.as_deref(),
            Some(DEV_MANIFEST_KEY_ID)
        );
        assert_eq!(
            manifest_report.compiled_trust_set_key_ids,
            trust_set_key_ids(compiled_manifest_trust_set())
        );

        let provenance = RungRecordProvenance {
            image_path: "/opt/cortexkit/aft-gh-shim".to_string(),
            version: "0.53.0-test".to_string(),
            repo_key: "cortexkit/aft".to_string(),
        };
        let determination =
            RungDetermination::r2(TEST_NOW, R2Reason::DaemonUnreachable, Some(1), &provenance);
        write_rung_record_silently(&paths, &determination.record);
        let fresh_rung = last_rung_report(&paths);
        assert_eq!(
            fresh_rung.recorded_by_image_path.as_deref(),
            Some("/opt/cortexkit/aft-gh-shim")
        );
        assert_eq!(
            fresh_rung.recorded_by_version.as_deref(),
            Some("0.53.0-test")
        );
        assert_eq!(
            fresh_rung.recorded_by_repo_key.as_deref(),
            Some("cortexkit/aft")
        );

        fs::write(
            &paths.rung,
            serde_json::to_vec(&json!({
                "rung": "R2",
                "as_of_unix_secs": TEST_NOW,
                "inputs": { "daemon_unreachable": "failed" },
                "manifest_version": 1
            }))
            .unwrap(),
        )
        .unwrap();
        let legacy_rung = last_rung_report(&paths);
        assert_eq!(
            legacy_rung.recorded_by_image_path.as_deref(),
            Some(PRE_PROVENANCE_RECORD)
        );
        assert_eq!(
            legacy_rung.recorded_by_version.as_deref(),
            Some(PRE_PROVENANCE_RECORD)
        );
        assert_eq!(
            legacy_rung.recorded_by_repo_key.as_deref(),
            Some(PRE_PROVENANCE_RECORD)
        );
    }

    #[test]
    fn trust_set_provenance_explains_image_level_untrusted_key_regression() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        write_signed_manifest(&paths, fixture_manifest(), TEST_NOW);
        let verifier_a = [Some(ManifestTrustKey {
            key_id: DEV_MANIFEST_KEY_ID,
            public_key: &DEV_MANIFEST_PUBLIC_KEY,
        })];
        let verifier_b = [Some(PROD_MANIFEST_TRUST_KEY)];

        let report_a = cached_manifest_report_at_with(&paths, TEST_NOW, &verifier_a);
        assert_eq!(report_a.state, Some("valid"));
        assert_eq!(
            report_a.verified_by_key_id.as_deref(),
            Some(DEV_MANIFEST_KEY_ID)
        );
        assert_eq!(
            report_a.compiled_trust_set_key_ids,
            vec![DEV_MANIFEST_KEY_ID]
        );

        let report_b = cached_manifest_report_at_with(&paths, TEST_NOW, &verifier_b);
        assert_eq!(report_b.state, Some("regressed"));
        assert_eq!(report_b.verified_by_key_id, None);
        assert_eq!(
            report_b.compiled_trust_set_key_ids,
            vec![PROD_MANIFEST_KEY_ID]
        );
        assert_eq!(
            report_b.diagnostic_guidance,
            Some(UNTRUSTED_MANIFEST_KEY_STEERING)
        );

        let cached = read_last_valid_manifest(&paths).expect("verifier A wrote last-valid cache");
        let governed = [
            OsString::from("issue"),
            OsString::from("comment"),
            OsString::from("42"),
            OsString::from("--body"),
            OsString::from("hello"),
        ];
        let untrusted =
            ManifestProblem::Invalid(format!("untrusted manifest key id {DEV_MANIFEST_KEY_ID}"));
        let RegressedDisposition::Refuse { text, .. } =
            regressed_disposition(&governed, &cached.manifest, "macos", &untrusted)
        else {
            panic!("a governed command must refuse under verifier B");
        };
        assert!(text.ends_with(UNTRUSTED_MANIFEST_KEY_STEERING));
    }

    #[test]
    fn version_high_water_refuses_rollbacks_and_status_reports_them() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());

        // Accept the newer manifest first.
        write_envelope_fixture(
            &paths,
            include_str!("../tests/fixtures/gh_shim/signed-envelope-v2-version-2.json"),
        );
        assert_eq!(load_manifest(&paths, TEST_NOW).unwrap().manifest_version, 2);
        assert_eq!(version_high_water(&paths), 2);

        // A validly-signed OLDER manifest is then refused as a rollback
        // incident, never as ordinary out-of-order arrival.
        write_envelope_fixture(
            &paths,
            include_str!("../tests/fixtures/gh_shim/signed-envelope-v2.json"),
        );
        assert!(matches!(
            load_manifest(&paths, TEST_NOW),
            Err(ManifestProblem::RolledBack {
                manifest_version: 1,
                newest_accepted: 2,
            })
        ));
        let report = cached_manifest_report_at(&paths, TEST_NOW);
        assert_eq!(
            report.diagnostics,
            vec![
                SelfReportDiagnostic::ManifestRegressed.as_str(),
                SelfReportDiagnostic::ManifestRollback.as_str(),
            ]
        );
        // That rollback is also visible through the --status document.
        let document = render_self_report(&paths).expect("self report");
        assert!(document.contains(SelfReportDiagnostic::ManifestRollback.as_str()));

        // Re-presenting the newest accepted version is not a rollback.
        write_envelope_fixture(
            &paths,
            include_str!("../tests/fixtures/gh_shim/signed-envelope-v2-version-2.json"),
        );
        assert_eq!(load_manifest(&paths, TEST_NOW).unwrap().manifest_version, 2);
    }

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gh_shim")
    }

    fn canonical_manifest_bytes() -> Vec<u8> {
        fs::read(fixture_dir().join("initial-manifest-v1.json"))
            .expect("canonical manifest fixture")
    }

    fn envelope_json(envelope: &SignedManifest) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(envelope).expect("envelope serialization");
        bytes.push(b'\n');
        bytes
    }

    /// Deterministic generator for every dev-signed envelope fixture. The
    /// canonical fixture's signature covers the exact bytes of the checked-in
    /// manifest file; variant fixtures re-sign their serialized variant bytes.
    fn generate_envelope_fixtures() -> Vec<(String, Vec<u8>)> {
        let sign = |bytes: &[u8], seed: &[u8; 32]| {
            let key = Ed25519KeyPair::from_seed_unchecked(seed).expect("fixture key");
            base64::engine::general_purpose::STANDARD.encode(key.sign(bytes).as_ref())
        };
        let envelope = |key_id: &str, seed: &[u8; 32], manifest_bytes: String| {
            envelope_json(&SignedManifest {
                artifact_id: MANIFEST_ARTIFACT_ID.to_string(),
                envelope_version: ENVELOPE_VERSION,
                key_id: key_id.to_string(),
                fetched_at_unix_secs: FIXTURE_ISSUED_AT,
                signature: sign(manifest_bytes.as_bytes(), seed),
                manifest_bytes,
            })
        };

        let canonical = canonical_manifest_bytes();
        let canonical_text = String::from_utf8(canonical.clone()).expect("UTF-8 manifest");
        let canonical_signature = sign(&canonical, &TEST_SEED);

        let mut fixtures = Vec::new();
        // Raw-bytes round-trip golden: signature over the published file.
        fixtures.push((
            "signed-envelope-v2.json".to_string(),
            envelope_json(&SignedManifest {
                artifact_id: MANIFEST_ARTIFACT_ID.to_string(),
                envelope_version: ENVELOPE_VERSION,
                key_id: DEV_MANIFEST_KEY_ID.to_string(),
                fetched_at_unix_secs: FIXTURE_ISSUED_AT,
                signature: canonical_signature.clone(),
                manifest_bytes: canonical_text.clone(),
            }),
        ));
        // Tampered-single-byte case: one substitution inside the signed bytes,
        // keeping the ORIGINAL signature so verification must fail.
        let tampered = canonical_text.replacen("issue view", "issue View", 1);
        assert_ne!(tampered, canonical_text);
        fixtures.push((
            "signed-envelope-v2-tampered.json".to_string(),
            envelope_json(&SignedManifest {
                artifact_id: MANIFEST_ARTIFACT_ID.to_string(),
                envelope_version: ENVELOPE_VERSION,
                key_id: DEV_MANIFEST_KEY_ID.to_string(),
                fetched_at_unix_secs: FIXTURE_ISSUED_AT,
                signature: canonical_signature,
                manifest_bytes: tampered,
            }),
        ));

        let mut variant = |name: &str, mutate: fn(&mut Manifest), seed: &[u8; 32], key_id: &str| {
            let mut manifest = fixture_manifest();
            mutate(&mut manifest);
            let bytes = serde_json::to_vec(&manifest).expect("variant manifest bytes");
            fixtures.push((
                name.to_string(),
                envelope(
                    key_id,
                    seed,
                    String::from_utf8(bytes).expect("UTF-8 variant bytes"),
                ),
            ));
        };
        variant(
            "signed-envelope-v2-future-issued-at.json",
            |manifest| {
                manifest.issued_at_unix_secs =
                    FIXTURE_ISSUED_AT + ISSUED_AT_FUTURE_SKEW.as_secs() + 3300;
            },
            &TEST_SEED,
            DEV_MANIFEST_KEY_ID,
        );
        variant(
            "signed-envelope-v2-stale-issued-at.json",
            |manifest| {
                manifest.issued_at_unix_secs = FIXTURE_ISSUED_AT - 2_000_000;
            },
            &TEST_SEED,
            DEV_MANIFEST_KEY_ID,
        );
        variant(
            "signed-envelope-v2-version-2.json",
            |manifest| {
                manifest.manifest_version = 2;
            },
            &TEST_SEED,
            DEV_MANIFEST_KEY_ID,
        );
        variant(
            "signed-envelope-v2-standby-key.json",
            |_manifest| {},
            &STANDBY_TEST_SEED,
            DEV_STANDBY_MANIFEST_KEY_ID,
        );
        fixtures
    }

    #[test]
    fn signed_envelope_fixtures_match_their_generator() {
        let regen = std::env::var_os("AFT_GH_SHIM_REGEN").is_some();
        for (name, bytes) in generate_envelope_fixtures() {
            let path = fixture_dir().join(&name);
            if regen {
                fs::write(&path, &bytes).expect("write fixture");
                continue;
            }
            let disk = fs::read(&path)
                .unwrap_or_else(|error| panic!("fixture {name} is missing: {error}"));
            assert_eq!(
                disk, bytes,
                "fixture {name} drifted from its generator; rerun with AFT_GH_SHIM_REGEN=1"
            );
        }
    }
}

#[cfg(test)]
mod github_read_mutation_tests {
    //! These tests exercise PRIVATE gh_shim internals (GovernedRequest,
    //! GithubReadMutation) and can only compile beside them. They originally
    //! lived in the integration tree and reached the lib suite through an
    //! include! - a shape that breaks the moment anyone registers the file in
    //! the integration crate, so they live here as an ordinary module now.
    use super::invalidate_successful_github_read_mutation_at;
    use super::{GithubReadMutation, GovernedRequest, RouteOutcome};
    use crate::db::github_read_cache::GithubReadResourceKind;

    use crate::db::github_read_cache::{
        lookup_github_read_cache_entry, upsert_github_read_cache_entry, GithubReadCacheKey,
    };
    use rusqlite::Connection;

    fn github_read_mutation_request(
        action: &str,
        repository: &str,
        resource_number: i64,
    ) -> GovernedRequest {
        let mut target = serde_json::Map::new();
        target.insert(
            "number".to_string(),
            serde_json::Value::String(resource_number.to_string()),
        );
        GovernedRequest {
            action: action.to_string(),
            target,
            body: serde_json::Map::new(),
            repository: Some(repository.to_string()),
            manifest_version: 1,
            edit_last: false,
        }
    }

    fn cache_key(repository: &str, resource_number: i64, identity: &str) -> GithubReadCacheKey {
        GithubReadCacheKey::new(
            GithubReadResourceKind::Issue,
            repository,
            resource_number,
            identity,
        )
    }

    fn write_cached_issue(
        conn: &Connection,
        repository: &str,
        resource_number: i64,
        identity: &str,
    ) {
        upsert_github_read_cache_entry(
            conn,
            &cache_key(repository, resource_number, identity),
            "# Cached issue\n",
            1_000,
        )
        .expect("write cached issue");
    }

    fn cached_issue_exists(
        conn: &Connection,
        repository: &str,
        resource_number: i64,
        identity: &str,
    ) -> bool {
        lookup_github_read_cache_entry(conn, &cache_key(repository, resource_number, identity))
            .expect("look up cached issue")
            .is_some()
    }

    #[test]
    fn successful_structured_comment_mutation_invalidates_the_touched_issue_for_all_identities() {
        let storage = tempfile::tempdir().expect("create storage");
        let conn = crate::db::open(&storage.path().join("aft.db")).expect("open cache database");
        write_cached_issue(&conn, "cortexkit/aft", 42, "principal:alice");
        write_cached_issue(&conn, "cortexkit/aft", 42, "principal:bob");

        let request = github_read_mutation_request("issue comment", "CortexKit/AFT", 42);
        let mutation = GithubReadMutation::from_governed_request(&request)
            .expect("structured issue comment has a cache resource");
        assert_eq!(mutation.normalized_repository, "cortexkit/aft");
        assert_eq!(mutation.resource_kind, GithubReadResourceKind::Issue);
        assert_eq!(mutation.resource_number, 42);

        invalidate_successful_github_read_mutation_at(
            storage.path(),
            Some(&mutation),
            &RouteOutcome::Result("comment created".to_string()),
        );

        assert!(
            !cached_issue_exists(&conn, "cortexkit/aft", 42, "principal:alice"),
            "a successful comment invalidates Alice's cached issue"
        );
        assert!(
            !cached_issue_exists(&conn, "cortexkit/aft", 42, "principal:bob"),
            "a successful comment invalidates every identity's cached issue"
        );
    }

    #[test]
    fn failed_structured_comment_mutation_does_not_invalidate_the_touched_issue() {
        let storage = tempfile::tempdir().expect("create storage");
        let conn = crate::db::open(&storage.path().join("aft.db")).expect("open cache database");
        write_cached_issue(&conn, "cortexkit/aft", 42, "principal:alice");

        let request = github_read_mutation_request("issue comment", "cortexkit/aft", 42);
        let mutation = GithubReadMutation::from_governed_request(&request)
            .expect("structured issue comment has a cache resource");
        invalidate_successful_github_read_mutation_at(
            storage.path(),
            Some(&mutation),
            &RouteOutcome::UpstreamError("comment rejected".to_string()),
        );

        assert!(
            cached_issue_exists(&conn, "cortexkit/aft", 42, "principal:alice"),
            "a failed mutation must preserve the cached issue"
        );
    }

    #[test]
    fn successful_mutation_for_a_different_issue_leaves_the_control_entry_intact() {
        let storage = tempfile::tempdir().expect("create storage");
        let conn = crate::db::open(&storage.path().join("aft.db")).expect("open cache database");
        write_cached_issue(&conn, "cortexkit/aft", 42, "principal:alice");
        write_cached_issue(&conn, "cortexkit/aft", 43, "principal:alice");

        let request = github_read_mutation_request("issue comment", "cortexkit/aft", 43);
        let mutation = GithubReadMutation::from_governed_request(&request)
            .expect("structured issue comment has a cache resource");
        invalidate_successful_github_read_mutation_at(
            storage.path(),
            Some(&mutation),
            &RouteOutcome::Result("comment created".to_string()),
        );

        assert!(
            cached_issue_exists(&conn, "cortexkit/aft", 42, "principal:alice"),
            "a mutation for another issue must not evict the control entry"
        );
        assert!(
            !cached_issue_exists(&conn, "cortexkit/aft", 43, "principal:alice"),
            "the successful mutation must still evict its own issue"
        );
    }
}
