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
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(test)]
use std::time::Instant;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
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
pub const OUTCOME_UNKNOWN_EXIT_STATUS: i32 = 87;
const UPSTREAM_FAILURE_EXIT_STATUS: i32 = 1;
const DISCOVERY_BUDGET: Duration = Duration::from_secs(2);
/// Backoff before the single retry of the whole discovery probe. A loaded host
/// can blow the per-stage budget before the daemon answers; the daemon is
/// usually reachable on the second attempt, so refuse only after both attempts
/// time out.
const DISCOVERY_RETRY_BACKOFF: Duration = Duration::from_millis(250);
const DISCOVERY_CACHE_TTL: Duration = Duration::from_secs(15);
const RECENTLY_REACHABLE_WINDOW: Duration = Duration::from_secs(300);
/// Clock skew tolerated before a manifest's signed issue time counts as being
/// in the future and therefore invalid.
const ISSUED_AT_FUTURE_SKEW: Duration = Duration::from_secs(300);
const ROUTING_OPERATION: &str = "gh.route";
const ROUTING_HOLDER_MODULE_ID: &str = "prefrontal-core";
const MANIFEST_ARTIFACT_ID: &str = "gh-routing-manifest";
const V1_GOVERNED_TUPLES: &[&str] = &["issue comment", "pr comment", "pr review", "issue reaction"];
const V1_ADMIN_TUPLES: &[&str] = &["issue close", "pr close", "pr merge", "release create"];
const V9_ADMIN_TUPLES: &[&str] = &["repo edit", "run delete"];
// v11 keeps the four thread-state verbs on the admin allowlist so a still-live
// v11 manifest (no canonicalization) continues to refuse them as admin until a
// signed v12 artifact moves them to governed.
const V11_ADMIN_TUPLES: &[&str] = &["issue reopen", "pr reopen"];
const V12_GOVERNED_TUPLES: &[&str] = &["issue close", "issue reopen", "pr close", "pr reopen"];
const TARGET_AND_STATE_FORM: &str = "target-and-state";
const ISSUE_CLOSE_REASONS: &[&str] = &["completed", "not_planned"];
// These v10 tuples are explicitly reviewed for the operator-only bypass and
// still require a matching signed manifest declaration. A rerun is
// administration rather than governed bot speech: it has no public attribution
// surface, while granting speech Apps actions:write would widen the compromise
// surface. v15 admits cancellation under the same audited operator authority:
// a hung CI run must not hold a seat until GitHub's timeout. `--force` still
// cancels only the selected run, so it has the same authority as plain cancel.
const V10_ADMIN_TUPLES: &[&str] = &["workflow run", "run rerun"];
const V15_ADMIN_TUPLES: &[&str] = &["run cancel"];
// v13 adds operator-only release maintenance while keeping release deletion
// and release delete-prefixed flags outside the bypass allowlist.
const V13_ADMIN_TUPLES: &[&str] = &["release edit", "release upload"];
// v14 admits three more shapes of bot speech: opening a thread, editing an
// issue the calling seat's bot already opened, and editing a comment the bot
// already wrote. They speak publicly under the bot identity and create no
// authority, which is the same class as `issue comment` and `issue close`.
const V14_GOVERNED_TUPLES: &[&str] = &["issue create", "issue edit"];
// v16 lets a bot open a pull request. Opening one proposes a change and lands
// nothing: merging stays on the admin `pr merge` row, so this is speech under
// the bot identity, the same class as `issue create`. The head branch must live
// in the target repository; a fork head (`owner:branch`) is refused.
const V16_GOVERNED_TUPLES: &[&str] = &["pr create"];
/// The body fields the `pr create` reader produces, in the order the manifest
/// declares them. The reader is purpose-built for this list, so a signed
/// declaration naming any other list is refused rather than half-honoured.
const PR_CREATE_BODY_FIELDS: &[&str] = &["title", "body", "base", "head", "draft"];
/// `gh pr create` flags the governed request cannot carry. Reviewers,
/// assignees, labels, milestones and projects hand out work or triage rather
/// than speak; `--fill*` derives text from local commits the route holder never
/// sees; `--web`, `--editor`, `--template` and `--recover` need an interactive
/// terminal; `--dry-run` asks upstream `gh` to print instead of create, which a
/// governed route cannot honour; `--no-maintainer-edit` changes a permission
/// the request has no field for. Short spellings are listed next to their long
/// forms so each gets the same named refusal.
const PR_CREATE_UNSUPPORTED_FLAGS: &[&str] = &[
    "--assignee",
    "-a",
    "--reviewer",
    "-r",
    "--label",
    "-l",
    "--milestone",
    "-m",
    "--project",
    "-p",
    "--fill",
    "-f",
    "--fill-first",
    "--fill-verbose",
    "--web",
    "-w",
    "--editor",
    "-e",
    "--template",
    "-T",
    "--recover",
    "--dry-run",
    "--no-maintainer-edit",
];
// v14 also gives the operator bypass one narrow administration shape of a
// governed verb: an `issue edit` that changes labels and nothing else. Triage
// labels on any issue are repository administration, not bot speech, so they
// run under the operator's own `gh` with an audit line, like `pr merge`. Every
// other `issue edit` argument (title, body, assignees, milestones, projects)
// is refused on this path, and without the bypass `issue edit` stays on the
// governed own-issue route.
const V14_OPERATOR_LABEL_TUPLES: &[&str] = &["issue edit"];
// v14 extends the same operator row to two verbs that have no bot-speech route
// at all. Maintainers gating design work put labels such as `design-approved`
// on issues and `trivial` on pull requests, and create those labels when a
// repository lacks them. The manifest declares them in the admin
// tier, but each runs only in its row's narrow shape: a label-only `pr edit` on
// any pull request, and a `label create` with a name, color, description and
// `--force`. Without the bypass both refuse as undeclared, exactly as before
// v14, and label deletion and editing stay undeclared.
const V14_OPERATOR_ROW_ADMIN_TUPLES: &[&str] = &["pr edit", "label create"];
/// The only API endpoint admitted as governed speech: the id-addressed edit of
/// an issue comment. The shim classifies and forwards; the route holder is what
/// verifies the comment was written by the calling seat's bot.
const OWN_COMMENT_PATCH_PATH_GLOB: &str = "/repos/*/*/issues/comments/*";
const OWN_COMMENT_PATCH_METHOD: &str = "PATCH";
/// Argv form for a governed verb that names no existing target: an issue does
/// not have a number until it is created, so the declaration carries body
/// fields and an empty target instead of a positional.
const FIELDS_ONLY_FORM: &str = "fields-only";
/// Argv form for the own-comment PATCH: the target comes from the endpoint path
/// and the only admitted payload field is the replacement body.
const BODY_ONLY_FORM: &str = "body-only";
/// `gh issue create` flags the shim refuses outright rather than forwarding.
/// Assignment, milestones and projects hand out work rather than speak;
/// `--web`, `--template` and `--recover` need an interactive terminal the
/// governed seam cannot reproduce.
const CREATE_UNSUPPORTED_FLAGS: &[&str] = &[
    "--assignee",
    "--milestone",
    "--project",
    "--web",
    "--template",
    "--recover",
];
/// `gh issue edit` planning flags are not bot speech. Refusing these as
/// destructive keeps repository planning changes out of the governed route.
const ISSUE_EDIT_DESTRUCTIVE_FLAGS: &[&str] = &[
    "--milestone",
    "--remove-milestone",
    "--project",
    "--add-project",
    "--remove-project",
];
const DESTRUCTIVE_TUPLES: &[&str] = &["release delete", "release delete-asset"];
// The v10 manifest version is the first version whose code-side allowlist
// permits these native comment mutations. The allowlist covers only the exact
// flag variants below and does not broaden raw API writes.
const V10_EDIT_LAST_TUPLES: &[&str] = &["issue comment", "pr comment"];
const READ_ONLY_ACTION_TUPLES: &[&str] = &[
    "issue view",
    "issue list",
    "issue status",
    "pr view",
    "pr list",
    "pr status",
    "pr checks",
    "pr diff",
    "release view",
    "release list",
    "release download",
    "repo view",
    "repo list",
    "run view",
    "run list",
    "run watch",
    "run download",
    "workflow view",
    "workflow list",
    "label list",
    "search issues",
    "search prs",
    "search repos",
    "search code",
    "search commits",
    "cache list",
];
/// Commands that may pass through to upstream `gh` even when the repository
/// they name has no bot binding. The check for unbound targets is a safe list:
/// anything not listed here or in `READ_ONLY_ACTION_TUPLES` (and not a `gh api`
/// read) is refused on an unbound target, including verbs and subcommands this
/// build does not know, so one added to a future `gh` cannot slip through under
/// the operator's login. Every entry is exact: `verb subcommand`, or a bare
/// `verb` for a command that takes no subcommand, matched as the shim reads
/// the first two positional words (`command_head`).
///
/// Not listed, on purpose: `extension exec` and any extension or alias invoked
/// by name. Both run code the shim cannot inspect (an extension is a program,
/// an alias can expand to `api --method POST` or to a shell command), and
/// either can write with the operator's token, so they are refused on an
/// unbound target like any unknown verb. Every `gh auth` subcommand other
/// than `auth status` is refused before this list is consulted (see
/// `operator_credential_use`).
const UNBOUND_SAFE_COMMANDS: &[&str] = &[
    // Reads beyond `READ_ONLY_ACTION_TUPLES`. They only fetch from GitHub.
    // (That table also feeds classification on bound repositories, so these
    // stay here instead of widening it.) `status` takes no subcommand; a
    // value flag such as `status -o org` reads as a subcommand and is refused.
    "search issues",
    "search prs",
    "search repos",
    "search code",
    "search commits",
    "status",
    "org list",
    "gist list",
    "gist view",
    "secret list",
    "variable list",
    "variable get",
    "ruleset list",
    "ruleset view",
    "ruleset check",
    "project list",
    "project view",
    "project field-list",
    "project item-list",
    "ssh-key list",
    "gpg-key list",
    "codespace list",
    "release verify",
    "release verify-asset",
    "attestation verify",
    "extension list",
    "extension search",
    // Local machine only. `auth status` without a token flag reports which
    // account is logged in. `config` and `alias` edit the operator's local
    // gh configuration; managing an alias does not run it. `completion`
    // prints a shell script (the shell name reads as its subcommand), and
    // `help` and `version` print text. None of them sends a write to GitHub.
    "auth status",
    "config get",
    "config set",
    "config list",
    "config clear-cache",
    "alias list",
    "alias set",
    "alias delete",
    "alias import",
    "completion",
    "completion bash",
    "completion zsh",
    "completion fish",
    "completion powershell",
    "version",
    // `help` alone, or `help <command or topic>`: the word names what to
    // describe, and help never runs it.
    "help",
    "help alias",
    "help api",
    "help attestation",
    "help auth",
    "help browse",
    "help cache",
    "help codespace",
    "help completion",
    "help config",
    "help environment",
    "help exit-codes",
    "help extension",
    "help formatting",
    "help gist",
    "help gpg-key",
    "help issue",
    "help label",
    "help mintty",
    "help org",
    "help pr",
    "help project",
    "help reference",
    "help release",
    "help repo",
    "help ruleset",
    "help run",
    "help search",
    "help secret",
    "help ssh-key",
    "help status",
    "help variable",
    "help workflow",
    // Local git work on a copy: cloning and checking out a pull request
    // fetch from GitHub and write only to the local disk.
    "repo clone",
    "gist clone",
    "pr checkout",
    // `browse` takes no subcommand; with `--no-browser` it prints a URL. The
    // flag check, and the limit of one location argument (an issue or pull
    // request number, a path or a commit), are in `is_unbound_safe`.
    "browse",
];
/// Writes that act on the caller's account rather than on an existing
/// repository: a new repository, a fork, gists, account keys and projects. No
/// manifest binding can cover them, so they are refused without the operator
/// bypass even from inside a bound checkout.
const ACCOUNT_WRITE_TUPLES: &[&str] = &[
    "repo create",
    "repo fork",
    "gist create",
    "gist edit",
    "gist delete",
    "gist rename",
    "ssh-key add",
    "ssh-key delete",
    "gpg-key add",
    "gpg-key delete",
    "project close",
    "project copy",
    "project create",
    "project delete",
    "project edit",
    "project field-create",
    "project field-delete",
    "project item-add",
    "project item-archive",
    "project item-create",
    "project item-delete",
    "project item-edit",
    "project link",
    "project mark-template",
    "project unlink",
];
/// Verbs whose third word picks the action (`gh repo deploy-key add`). Their
/// `list` and `view` actions read; every other action writes.
const NESTED_ACTION_GROUPS: &[&str] = &["repo deploy-key", "repo autolink"];
/// `gh repo` subcommands whose first positional names the repository they act
/// on (`gh repo delete owner/name`). `repo rename` is absent: its positional is
/// the new name, and `-R` names the repository.
const REPO_POSITIONAL_TARGET_SUBCOMMANDS: &[&str] = &[
    "view",
    "clone",
    "create",
    "delete",
    "edit",
    "fork",
    "archive",
    "unarchive",
    "sync",
    "set-default",
];
/// Flags of the `gh repo` subcommands above whose next argument is their
/// value, so a description such as `--description a/b` is not read as the
/// repository.
const REPO_VALUE_FLAGS: &[&str] = &[
    "--description",
    "-d",
    "--homepage",
    "-h",
    "--source",
    "-s",
    "--remote",
    "-r",
    "--team",
    "-t",
    "--template",
    "-p",
    "--gitignore",
    "-g",
    "--license",
    "-l",
    "--visibility",
    "--default-branch",
    "--add-topic",
    "--remove-topic",
    "--fork-name",
    "--org",
    "--remote-name",
    "--upstream-remote-name",
    "-u",
    "--branch",
    "-b",
    "--json",
    "--jq",
    "-q",
    "--repo",
    "-R",
    "--hostname",
    "--config-dir",
];
const RESERVED_SELF_REPORT: &[&str] = &["--status", "--shim-version"];
const CO_AUTHOR_LINE_REPORT: &str = "--co-author-line";
const GOVERNANCE_UNAVAILABLE_TEXT: &str = "the governance daemon is unreachable and this repository's actions are identity-governed; retry after the daemon returns";
/// Human-readable text for a local discovery-probe deadline expiry. A deadline
/// that expires on a loaded host does not mean the daemon is unreachable, so
/// this arm must not say "unreachable". The classification stays
/// `gh_shim_governance_unavailable` (exit 86) so consumers that distinguish
/// governance unavailability from other refusals keep working unchanged.
fn governance_probe_timeout_text(elapsed_ms: u64, stage: ProbeStage) -> String {
    format!(
        "governance probe timed out after {elapsed_ms} ms at {stage} (daemon may be busy; host load?) - this repository's actions are identity-governed, so the command was not run; retry"
    )
}
const UNTRUSTED_MANIFEST_KEY_STEERING: &str = "the manifest may be newer than this aft build's trust set - update aft, or install a manifest signed by a trusted key";
const PRE_PROVENANCE_RECORD: &str = "unrecorded (pre-provenance record)";
const GH_SHIM_STATE_DIR_ENV: &str = "AFT_GH_SHIM_STATE_DIR";

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
    MissingReason,
    DestructiveFlag,
    UnsupportedFlag,
    OutcomeUnknown,
    UnboundTarget,
    OperatorCredentials,
}

impl RefusalCode {
    pub const ALL: [Self; 17] = [
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
        Self::MissingReason,
        Self::DestructiveFlag,
        Self::UnsupportedFlag,
        Self::OutcomeUnknown,
        Self::UnboundTarget,
        Self::OperatorCredentials,
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
            Self::MissingReason => "gh_shim_missing_reason",
            Self::DestructiveFlag => "gh_shim_destructive_flag",
            Self::UnsupportedFlag => "gh_shim_unsupported_flag",
            Self::OutcomeUnknown => "gh_shim_outcome_unknown",
            Self::UnboundTarget => "gh_shim_unbound_target",
            Self::OperatorCredentials => "gh_shim_operator_credentials",
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

    // Agents run `gh auth status` to learn whether they can write. Upstream
    // `gh` would describe the operator's login, which is not the identity a
    // governed write uses, so the shim answers from its own local state. This
    // comes before the manifest arms below because it only reports and never
    // runs anything.
    if is_local_auth_status(args) {
        return dispatch_auth_status(
            args,
            &paths,
            now,
            read_user_config_doc().as_deref(),
            || {
                // The same resolver a write uses when it names no repository:
                // `GH_REPO`, else the working directory's origin remote.
                TargetRepository {
                    gh_repo: gh_repo_env(),
                    ..TargetRepository::default()
                }
                .repository_key(&cwd)
            },
            delegate,
        );
    }

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

    let target = TargetRepository::from_invocation(args);
    // A write aimed at a repository no signed binding covers cannot be spoken
    // by any bot, and upstream `gh` would run it under the operator's own
    // login. Decide that before the rung probe: the answer does not depend on
    // the governance daemon, and a bound target falls through to the governed
    // path below unchanged. Without a signed manifest (a public installation,
    // or one that never verified) there are no bindings to compare against,
    // so those keep passing through as before.
    if let ManifestResolution::Active(manifest) = &initial_manifest {
        // The operator's token works on every repository, bound or not, and
        // an agent holding it could reach GitHub around the shim, so this
        // check comes before any target is resolved.
        if let Some((command, credential_use)) =
            operator_credential_use(args).filter(|_| !shim_disabled_by_operator())
        {
            return dispatch_operator_credentials(
                args,
                &command,
                credential_use,
                &paths,
                now,
                delegate,
            );
        }
        if let Some(write) = unbound_write(args, manifest, current_platform(), &target, &cwd)
            .filter(|_| !shim_disabled_by_operator())
        {
            return dispatch_unbound_write(args, &write, &paths, now, delegate);
        }
    }
    let determination = determine_rung(&paths, &target, &cwd, now);
    if determination.record.rung != Rung::R3 {
        let disposition = match resolve_manifest(&paths, now) {
            ManifestResolution::Active(manifest) => non_r3_governance_disposition(
                &cwd,
                &target,
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
                let refusal_text = determination
                    .refusal_detail
                    .as_deref()
                    .unwrap_or(GOVERNANCE_UNAVAILABLE_TEXT);
                refuse_governance_unavailable(&paths, &agent_binding, now, refusal_text)
            }
            GovernanceDisposition::Unclassified { manifest_version } => refuse(
                RefusalCode::Unclassified,
                &unclassified_refusal_text(args, manifest_version),
            ),
            GovernanceDisposition::Destructive => refuse(
                RefusalCode::DestructiveFlag,
                "destructive GitHub operations are not available through the shim",
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
    let classification = classify(args, &manifest, current_platform());
    let Some(agent_binding) = governing_binding(&classification, &manifest, &target, &cwd) else {
        return delegate(args);
    };

    dispatch_r3(
        args,
        classification,
        &manifest,
        &paths,
        &determination.record,
        &agent_binding,
        now,
        delegate,
    )
}

#[allow(clippy::too_many_arguments)]
fn dispatch_r3<F>(
    args: &[OsString],
    classification: Classification,
    manifest: &Manifest,
    paths: &StatePaths,
    rung: &RungRecord,
    agent_binding: &AgentBinding,
    now: u64,
    delegate_to_upstream: F,
) -> i32
where
    F: FnOnce(&[OsString]) -> i32,
{
    dispatch_r3_with_relay(
        args,
        classification,
        manifest,
        paths,
        rung,
        agent_binding,
        now,
        delegate_to_upstream,
        &RelayContext::from_process(),
    )
}

#[allow(clippy::too_many_arguments)]
fn dispatch_r3_with_relay<F>(
    args: &[OsString],
    classification: Classification,
    manifest: &Manifest,
    paths: &StatePaths,
    rung: &RungRecord,
    agent_binding: &AgentBinding,
    now: u64,
    delegate_to_upstream: F,
    relay: &RelayContext,
) -> i32
where
    F: FnOnce(&[OsString]) -> i32,
{
    dispatch_r3_with_relay_at(
        args,
        classification,
        manifest,
        paths,
        rung,
        agent_binding,
        now,
        delegate_to_upstream,
        relay,
        &crate::bash_background::storage_dir(None),
    )
}

// Relay fixtures need the same dispatch and invalidation path without inheriting
// the operator's database. Production keeps resolving its ordinary shared root.
#[allow(clippy::too_many_arguments)]
fn dispatch_r3_with_relay_at<F>(
    args: &[OsString],
    classification: Classification,
    manifest: &Manifest,
    paths: &StatePaths,
    rung: &RungRecord,
    agent_binding: &AgentBinding,
    now: u64,
    delegate_to_upstream: F,
    relay: &RelayContext,
    storage_root: &Path,
) -> i32
where
    F: FnOnce(&[OsString]) -> i32,
{
    match classification {
        Classification::Mechanical => delegate_to_upstream(args),
        Classification::Admin { tuple } => {
            if is_reviewed_operator_row_admin_tuple(manifest.manifest_version, &tuple) {
                return dispatch_operator_row_admin(
                    args,
                    &tuple,
                    manifest.manifest_version,
                    paths,
                    now,
                    delegate_to_upstream,
                );
            }
            if operator_bypass_requested() {
                let repository = std::env::current_dir()
                    .ok()
                    .and_then(|cwd| TargetRepository::from_invocation(args).resolve(&cwd));
                if let Err(error) = append_bypass_audit(paths, &tuple, repository.as_deref(), now) {
                    return refuse(
                        RefusalCode::BypassAuditUnavailable,
                        &format!("operator bypass audit could not be appended: {error}"),
                    );
                }
                delegate_to_upstream(args)
            } else {
                refuse(RefusalCode::AdminTier, &admin_refusal_text(&tuple))
            }
        }
        Classification::Governed { tuple, canonical } => {
            // The classification already proves the manifest declares this
            // tuple at a version that reviewed it; the label row adds its own
            // version check so the bypass cannot reach an older declaration.
            if operator_bypass_requested()
                && is_reviewed_operator_label_tuple(manifest.manifest_version, &tuple)
            {
                return dispatch_operator_label_edit(
                    args,
                    &tuple,
                    LabelTarget::Issue,
                    paths,
                    now,
                    delegate_to_upstream,
                );
            }
            // An API row is addressed by endpoint rather than by subcommand and
            // positional, so it has its own argv reader.
            let canonicalized = if is_api_tuple(&tuple) {
                canonicalize_governed_api(args, &tuple, &canonical, manifest.manifest_version)
            } else {
                canonicalize_governed(args, &tuple, &canonical, manifest.manifest_version)
            };
            let request = match canonicalized {
                Ok(request) => request,
                Err(error) => return refuse_governed_canonicalization(&error),
            };
            // The binding was chosen for the repository the command appeared
            // to target; the canonical request is the authoritative reading.
            // Were they to differ, the request would speak in one repository
            // as another repository's bot, so refuse instead.
            if request.repository.as_deref() != Some(agent_binding.repo.as_str()) {
                return refuse_governed_canonicalization(&CanonicalizeError::unclassified(
                    format!(
                        "the command targets {} but was resolved to {}'s binding; name one repository with --repo",
                        request.repository.as_deref().unwrap_or("no repository"),
                        agent_binding.repo
                    ),
                ));
            }
            let mutation = GithubReadMutation::from_governed_request(&request);
            let outcome = route_governed(paths, rung, agent_binding, request, now, manifest, relay);
            invalidate_successful_github_read_mutation_at(
                storage_root,
                mutation.as_ref(),
                &outcome,
            );
            governed_outcome_status(paths, agent_binding, now, outcome)
        }
        Classification::Unclassified => refuse(
            RefusalCode::Unclassified,
            &unclassified_refusal_text(args, manifest.manifest_version),
        ),
        Classification::Destructive => refuse(
            RefusalCode::DestructiveFlag,
            "destructive GitHub operations are not available through the shim",
        ),
    }
}

/// True when the operator asked to run administration under their own `gh`.
fn operator_bypass_requested() -> bool {
    std::env::var_os("GH_SHIM_BYPASS").as_deref() == Some(OsStr::new("operator"))
}

/// Run an admin-tier operator row (`pr edit`, `label create`).
///
/// These verbs have no bot-speech route, so without the bypass they refuse as
/// undeclared, the same refusal code they had before v14 declared them. With
/// the bypass each runs only in its row's shape.
fn dispatch_operator_row_admin<F>(
    args: &[OsString],
    tuple: &str,
    manifest_version: u64,
    paths: &StatePaths,
    now: u64,
    delegate_to_upstream: F,
) -> i32
where
    F: FnOnce(&[OsString]) -> i32,
{
    let row_text = match tuple {
        "pr edit" => LabelTarget::PullRequest.row_text(),
        _ => OPERATOR_LABEL_CREATE_ROW_TEXT,
    };
    if !operator_bypass_requested() {
        return refuse(
            RefusalCode::Unclassified,
            &format!(
                "verb \"{tuple}\" has no bot-speech route in manifest {manifest_version}; {row_text}"
            ),
        );
    }
    match tuple {
        "pr edit" => dispatch_operator_label_edit(
            args,
            tuple,
            LabelTarget::PullRequest,
            paths,
            now,
            delegate_to_upstream,
        ),
        _ => dispatch_operator_label_create(args, tuple, paths, now, delegate_to_upstream),
    }
}

/// Run a `gh label create` as the operator.
///
/// The argv must be exactly the label-create row (see
/// `parse_operator_label_create`); anything else refuses by name without
/// touching the audit or upstream. The audit record is appended and synced
/// before upstream `gh` is spawned, so an attempt that crashes mid-call is
/// still on record.
fn dispatch_operator_label_create<F>(
    args: &[OsString],
    tuple: &str,
    paths: &StatePaths,
    now: u64,
    delegate_to_upstream: F,
) -> i32
where
    F: FnOnce(&[OsString]) -> i32,
{
    let create = match parse_operator_label_create(args) {
        Ok(create) => create,
        Err(error) => return refuse_governed_canonicalization(&error),
    };
    let repository = create.repository.clone().or_else(infer_repository_from_git);
    if let Err(error) = append_bypass_audit_record(
        paths,
        &json!({
            "as_of_unix_secs": now,
            "tuple": tuple,
            "repository": repository,
            "label": create.label,
            "color": create.color,
        }),
    ) {
        return refuse(
            RefusalCode::BypassAuditUnavailable,
            &format!("operator bypass audit could not be appended: {error}"),
        );
    }
    delegate_to_upstream(args)
}

/// Run a label-only `gh issue edit` or `gh pr edit` as the operator.
///
/// The argv must be exactly the label row (see `parse_operator_label_edit`);
/// anything else refuses by name without touching the audit or upstream. The
/// audit record is appended and synced before upstream `gh` is spawned, so an
/// attempt that crashes mid-call is still on record.
fn dispatch_operator_label_edit<F>(
    args: &[OsString],
    tuple: &str,
    target: LabelTarget,
    paths: &StatePaths,
    now: u64,
    delegate_to_upstream: F,
) -> i32
where
    F: FnOnce(&[OsString]) -> i32,
{
    let edit = match parse_operator_label_edit(args, target) {
        Ok(edit) => edit,
        Err(error) => return refuse_governed_canonicalization(&error),
    };
    let repository = edit.repository.clone().or_else(infer_repository_from_git);
    if let Err(error) = append_label_bypass_audit(paths, tuple, repository.as_deref(), &edit, now) {
        return refuse(
            RefusalCode::BypassAuditUnavailable,
            &format!("operator bypass audit could not be appended: {error}"),
        );
    }
    delegate_to_upstream(args)
}

/// Refusal text for an administration-tier verb reached without the operator
/// bypass.
///
/// It names the verb and describes the tier rather than the mechanism. The
/// bypass is the sanctioned, audited way to run these verbs, so the text must
/// not read as a prohibition or as a bare instruction to set an environment
/// variable: what actually changes is whose identity the call runs under.
fn admin_refusal_text(tuple: &str) -> String {
    format!(
        "`{tuple}` is administration-tier — it runs under the operator's identity, not the bot's. Re-run with GH_SHIM_BYPASS=operator; the shim records an operator-attributed audit line."
    )
}

/// A write whose target is not a repository bound in the signed manifest.
#[derive(Debug, Eq, PartialEq)]
struct UnboundWrite {
    /// The command as the refusal and the audit line name it: the verb tuple
    /// (`repo create`), or `api:<METHOD>:<endpoint>` for a raw API call.
    command: String,
    target: WriteTarget,
}

/// What a write invocation acts on, as far as the shim can tell before it runs.
#[derive(Debug, Eq, PartialEq)]
enum WriteTarget {
    /// An existing github.com repository, as canonical `owner/name`.
    Repository(String),
    /// Not an existing repository: a repository being created, a fork, the
    /// caller's gists, keys or projects, or an API endpoint outside
    /// `/repos/<owner>/<name>`. `description` completes the sentence
    /// "`<command>` ...", and `named` is the repository the command spells,
    /// if it spells one, for the audit line.
    NotARepository {
        description: String,
        named: Option<String>,
    },
    /// Nothing identifies the repository; the text says why.
    Undetermined(String),
}

impl WriteTarget {
    fn audit_repository(&self) -> Option<&str> {
        match self {
            Self::Repository(repository) => Some(repository),
            Self::NotARepository { named, .. } => named.as_deref(),
            Self::Undetermined(_) => None,
        }
    }
}

/// True when the user turned the shim off (`github.shim: false`), the same
/// operator hard-off the rung determination honours.
fn shim_disabled_by_operator() -> bool {
    gh_shim_enabled_from_config_doc(read_user_config_doc().as_deref().unwrap_or("")) == Some(false)
}

/// The write in `args` when no signed binding covers what it targets.
///
/// Returns `None`, leaving the invocation to the governed path, for a read, a
/// local-only command, a destructive form (which that path refuses whatever
/// it targets), and a write whose target is bound. A write that is not bot
/// speech (administration, or a verb the manifest does not declare) aimed at
/// another repository from inside a bound checkout also stays on that path,
/// which already sends it through the checkout's audited bypass or refuses it
/// (see `target_or_checkout_binding`). Writes that act on the
/// caller's account rather than a repository never have a binding, so the
/// checkout does not matter for them.
fn unbound_write(
    args: &[OsString],
    manifest: &Manifest,
    platform: &str,
    target: &TargetRepository,
    cwd: &Path,
) -> Option<UnboundWrite> {
    if is_unbound_safe(args) {
        return None;
    }
    let classification = classify(args, manifest, platform);
    if matches!(classification, Classification::Destructive) {
        return None;
    }
    let write_target = write_target(args, target, cwd);
    if !matches!(write_target, WriteTarget::NotARepository { .. })
        && governing_binding(&classification, manifest, target, cwd).is_some()
    {
        return None;
    }
    Some(UnboundWrite {
        command: write_command(args),
        target: write_target,
    })
}

/// Refuse a write on an unbound target, or run it as the operator under
/// `GH_SHIM_BYPASS=operator` after the same audit line an administration
/// bypass writes. The audit line is synced before upstream `gh` is spawned, so
/// an attempt that dies mid-call is still on record.
fn dispatch_unbound_write<F>(
    args: &[OsString],
    write: &UnboundWrite,
    paths: &StatePaths,
    now: u64,
    delegate_to_upstream: F,
) -> i32
where
    F: FnOnce(&[OsString]) -> i32,
{
    if !operator_bypass_requested() {
        return refuse(
            RefusalCode::UnboundTarget,
            &unbound_target_refusal_text(write),
        );
    }
    if let Err(error) =
        append_bypass_audit(paths, &write.command, write.target.audit_repository(), now)
    {
        return refuse(
            RefusalCode::BypassAuditUnavailable,
            &format!("operator bypass audit could not be appended: {error}"),
        );
    }
    delegate_to_upstream(args)
}

/// Refusal text for a write whose target no bot is bound to. It names the
/// command and the target, says why no bot can speak there, and gives the
/// operator's way to approve it.
fn unbound_target_refusal_text(write: &UnboundWrite) -> String {
    let command = &write.command;
    let subject = match &write.target {
        WriteTarget::Repository(repository) => format!(
            "`{command}` targets {repository}, which is not a bot-bound repository (the signed gh routing manifest binds no bot to it)"
        ),
        WriteTarget::NotARepository { description, .. } => format!(
            "`{command}` {description}, which is not a bot-bound repository (no manifest binding can cover it)"
        ),
        WriteTarget::Undetermined(reason) => format!(
            "`{command}` has no determinable target repository ({reason}), so it cannot be shown to be a bot-bound repository"
        ),
    };
    format!(
        "{subject}; bot speech is not possible there, and upstream gh would run it under the operator's own login. The operator can approve it: re-run with GH_SHIM_BYPASS=operator, and the shim records an operator-attributed audit line."
    )
}

/// The name a write goes by in its refusal and audit line.
fn write_command(args: &[OsString]) -> String {
    let Some((verb, subcommand, head_index)) = command_head(args) else {
        return "this invocation".to_string();
    };
    if verb == "api" {
        return match api_request_shape(&args[head_index..]) {
            Some(shape) => format!("api:{}:{}", shape.effective_method(), shape.path),
            None => verb,
        };
    }
    verb_tuple(verb, subcommand)
}

/// True when `args` may run under the operator's login even though what it
/// names has no bot binding: a known read, a command that acts only on the
/// local machine (`UNBOUND_SAFE_COMMANDS`), or a `gh api` read. Everything
/// else, including a verb or subcommand this build does not recognise and any
/// `gh auth` use that reveals or changes the operator's credentials, is not
/// safe.
fn is_unbound_safe(args: &[OsString]) -> bool {
    if has_exact_flag(args, "--help") {
        // Upstream `gh` prints help and runs nothing.
        return true;
    }
    let Some((verb, subcommand, head_index)) = command_head(args) else {
        // No verb: upstream `gh` prints help or its version. An argument the
        // shim cannot read might hide a verb, so it is not safe.
        return args.iter().all(|arg| arg.to_str().is_some());
    };
    // A flag before the verb is one the shim does not model: its value could
    // be what `command_head` took for the verb, so the real verb is unknown.
    if args[..head_index]
        .iter()
        .any(|arg| arg.to_str().is_none_or(|value| value.starts_with('-')))
    {
        return false;
    }
    if operator_credential_use(args).is_some() {
        return false;
    }
    if verb == "api" {
        return !api_invocation_writes(&args[head_index..]);
    }
    if verb == "browse" {
        // One location argument at most (read as the subcommand); a second
        // positional is something the shim does not model.
        return has_exact_flag(args, "--no-browser") && nested_action(args).is_none();
    }
    let tuple = verb_tuple(verb, subcommand);
    if NESTED_ACTION_GROUPS.contains(&tuple.as_str()) {
        return matches!(nested_action(args).as_deref(), Some("list" | "view"));
    }
    READ_ONLY_ACTION_TUPLES.contains(&tuple.as_str())
        || UNBOUND_SAFE_COMMANDS.contains(&tuple.as_str())
}

/// How a `gh auth` command touches the operator's credentials.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CredentialUse {
    /// Prints the operator's token (`auth token`, `auth status --show-token`).
    /// With it an agent could call the GitHub API directly, around the shim.
    RevealsToken,
    /// Logs in or out, refreshes or switches accounts, or rewrites git's
    /// credential configuration (`auth login`, `setup-git`, and any `auth`
    /// subcommand this build does not know).
    ChangesCredentials,
}

/// The `gh auth` command in `args` and how it touches the operator's
/// credentials, or `None` when it does not: `auth status` without a token
/// flag, `gh auth` alone (help), or any other verb.
fn operator_credential_use(args: &[OsString]) -> Option<(String, CredentialUse)> {
    if has_exact_flag(args, "--help") {
        return None;
    }
    let (verb, subcommand, head_index) = command_head(args)?;
    if verb != "auth" {
        return None;
    }
    let subcommand = subcommand?;
    let tuple = format!("auth {subcommand}");
    match subcommand.as_str() {
        "status" if !shows_token(&args[head_index..]) => None,
        "status" | "token" => Some((tuple, CredentialUse::RevealsToken)),
        _ => Some((tuple, CredentialUse::ChangesCredentials)),
    }
}

/// True when `gh auth status` is asked to print the token: `--show-token`
/// (unless set to false), or `-t` alone or in a cluster of short flags (`-at`).
fn shows_token(args: &[OsString]) -> bool {
    args.iter().filter_map(|arg| arg.to_str()).any(|value| {
        if value == "--show-token" {
            return true;
        }
        if let Some(setting) = value.strip_prefix("--show-token=") {
            return !setting.eq_ignore_ascii_case("false");
        }
        value
            .strip_prefix('-')
            .filter(|cluster| !cluster.starts_with('-'))
            .is_some_and(|cluster| {
                cluster.chars().all(|flag| flag.is_ascii_alphabetic()) && cluster.contains('t')
            })
    })
}

/// Refuse a `gh auth` command that reveals or changes the operator's
/// credentials, or run it under `GH_SHIM_BYPASS=operator` after an audit line.
/// This holds whatever repository the command runs in: the token works on
/// every repository, bound or not.
fn dispatch_operator_credentials<F>(
    args: &[OsString],
    command: &str,
    credential_use: CredentialUse,
    paths: &StatePaths,
    now: u64,
    delegate_to_upstream: F,
) -> i32
where
    F: FnOnce(&[OsString]) -> i32,
{
    if !operator_bypass_requested() {
        return refuse(
            RefusalCode::OperatorCredentials,
            &operator_credentials_refusal_text(command, credential_use),
        );
    }
    if let Err(error) = append_bypass_audit(paths, command, None, now) {
        return refuse(
            RefusalCode::BypassAuditUnavailable,
            &format!("operator bypass audit could not be appended: {error}"),
        );
    }
    delegate_to_upstream(args)
}

fn operator_credentials_refusal_text(command: &str, credential_use: CredentialUse) -> String {
    let effect = match credential_use {
        CredentialUse::RevealsToken => "prints the operator's GitHub token into this agent's session, and with it an agent could call the GitHub API directly, around the shim",
        CredentialUse::ChangesCredentials => "changes the operator's gh login or git credential configuration",
    };
    format!(
        "`{command}` {effect}. The operator can approve it: re-run with GH_SHIM_BYPASS=operator, and the shim records an operator-attributed audit line."
    )
}

/// How the shim answers `gh auth status`.
#[derive(Debug, Eq, PartialEq)]
enum AuthStatusAnswer {
    /// Governance is not in force here: the operator turned the shim off, or
    /// no signed manifest is installed (a public installation). The real `gh`
    /// then reports the operator's own login, exactly as without the shim.
    PassThrough,
    /// A flag or argument the shim does not answer. It is refused by name
    /// rather than handed to the real `gh`, whose output would describe the
    /// operator's login instead of the identity governed writes use.
    UnsupportedFlag(String),
    /// The locally rendered report. The exit status is 0 when governed writes
    /// are available in this repository and 1 otherwise, mirroring the "not
    /// logged in" status of upstream `gh` so scripts can test it.
    Report { text: String, exit_code: i32 },
}

/// True for `gh auth status` without a token flag. The token forms (`-t`,
/// `--show-token`) stay on the operator-credential refusal path, and
/// `--help` stays with the real `gh`, which only prints help.
fn is_local_auth_status(args: &[OsString]) -> bool {
    if has_exact_flag(args, "--help") {
        return false;
    }
    let Some((verb, Some(subcommand), head_index)) = command_head(args) else {
        return false;
    };
    verb == "auth" && subcommand == "status" && !shows_token(&args[head_index..])
}

/// Refusal text for an `auth status` argument the shim does not answer, or
/// `None` when the invocation is `auth status` optionally followed by
/// `-h`/`--hostname github.com`.
fn auth_status_argument_refusal(args: &[OsString]) -> Option<String> {
    const ANSWERED: &str = "the shim answers `gh auth status` itself, optionally with `--hostname github.com`, and does not pass other forms to the real gh, whose answer would describe the operator's login rather than the identity governed writes use";
    let mut positionals = 0;
    let mut arguments = args.iter();
    while let Some(argument) = arguments.next() {
        let Some(value) = argument.to_str() else {
            return Some(format!(
                "`auth status` was given an argument that is not valid UTF-8; {ANSWERED}"
            ));
        };
        let host = if value == "-h" || value == "--hostname" {
            match arguments.next().and_then(|host| host.to_str()) {
                Some(host) => Some(host),
                None => {
                    return Some(format!(
                        "`auth status {value}` needs a host name; {ANSWERED}"
                    ))
                }
            }
        } else {
            value.strip_prefix("--hostname=")
        };
        if let Some(host) = host {
            if !host.eq_ignore_ascii_case("github.com") {
                return Some(format!(
                    "`auth status --hostname {host}` asks about a host the shim does not govern (it governs github.com only); {ANSWERED}"
                ));
            }
            continue;
        }
        if value.starts_with('-') {
            return Some(format!(
                "`auth status {}` is not supported by the shim; {ANSWERED}",
                refused_flag_name(value)
            ));
        }
        // The first two positionals are `auth` and `status` themselves.
        positionals += 1;
        if positionals > 2 {
            return Some(format!(
                "`auth status` takes no argument, but was given `{value}`; {ANSWERED}"
            ));
        }
    }
    None
}

/// Run `gh auth status` through `answer_auth_status`: print the report (to
/// stdout when governed writes are available, to stderr otherwise, as
/// upstream `gh` does), refuse an unsupported form, or delegate when
/// governance is not in force.
fn dispatch_auth_status<R, F>(
    args: &[OsString],
    paths: &StatePaths,
    now: u64,
    config_doc: Option<&str>,
    resolve_repository: R,
    delegate_to_upstream: F,
) -> i32
where
    R: FnOnce() -> Option<String>,
    F: FnOnce(&[OsString]) -> i32,
{
    match answer_auth_status(args, paths, now, config_doc, resolve_repository) {
        AuthStatusAnswer::PassThrough => delegate_to_upstream(args),
        AuthStatusAnswer::UnsupportedFlag(text) => refuse(RefusalCode::UnsupportedFlag, &text),
        AuthStatusAnswer::Report { text, exit_code } => {
            if exit_code == 0 {
                print!("{text}");
            } else {
                eprint!("{text}");
            }
            exit_code
        }
    }
}

/// Answer `gh auth status` from local state only: the user config, the
/// installed manifest, and the last recorded rung. It never runs the real
/// `gh` and never contacts the governance daemon or GitHub, so the routing
/// line reports the last determination, as `gh --status` does, rather than
/// probing. `resolve_repository` yields the canonical `owner/name` a write
/// that names no repository would target from the working directory.
fn answer_auth_status<R>(
    args: &[OsString],
    paths: &StatePaths,
    now: u64,
    config_doc: Option<&str>,
    resolve_repository: R,
) -> AuthStatusAnswer
where
    R: FnOnce() -> Option<String>,
{
    // With the shim turned off, or no manifest installed, writes already run
    // as the operator through the real `gh`, so its own answer is the true one.
    if gh_shim_enabled_from_config_doc(config_doc.unwrap_or("")) == Some(false) {
        return AuthStatusAnswer::PassThrough;
    }
    let manifest = resolve_manifest(paths, now);
    if matches!(manifest, ManifestResolution::Dormant) {
        return AuthStatusAnswer::PassThrough;
    }
    if let Some(text) = auth_status_argument_refusal(args) {
        return AuthStatusAnswer::UnsupportedFlag(text);
    }

    let repository = resolve_repository();
    let mut unavailable = Vec::new();

    let manifest_line = match &manifest {
        ManifestResolution::Active(manifest) => {
            format!("version {}, signature verified", manifest.manifest_version)
        }
        ManifestResolution::Regressed { manifest, problem } => {
            unavailable.push(format!(
                "the installed routing manifest failed verification ({}), so governed writes are refused until a valid manifest is installed",
                problem.status_label()
            ));
            format!(
                "failed verification: {} (last valid version {} is cached)",
                problem.status_label(),
                manifest.manifest_version
            )
        }
        ManifestResolution::Invalid(problem) => {
            unavailable.push(format!(
                "the installed routing manifest failed verification ({})",
                problem.status_label()
            ));
            format!("failed verification: {}", problem.status_label())
        }
        ManifestResolution::Dormant => unreachable!("a dormant shim passes auth status through"),
    };

    let identity_line = match (&manifest, repository.as_deref()) {
        (_, None) => {
            unavailable.push(
                "no repository: this directory has no github.com origin remote and GH_REPO is unset, so no bot binding applies here (a write that names a bound repository with --repo uses that repository's bot)"
                    .to_string(),
            );
            "X Governed writes: none (no repository)".to_string()
        }
        (ManifestResolution::Active(manifest), Some(repository)) => {
            match manifest.bindings.get(repository) {
                Some(agent_id) => format!(
                    "\u{2713} Governed writes: as {agent_id} (the signed routing manifest binds {repository} to it)"
                ),
                None => {
                    unavailable.push(format!(
                        "{repository} is not bound to a bot in the signed routing manifest, so writes there are refused unless the operator approves them with GH_SHIM_BYPASS=operator"
                    ));
                    format!(
                        "X Governed writes: none ({repository} is unbound; writes are refused unless the operator bypass applies)"
                    )
                }
            }
        }
        (_, Some(_)) => "X Governed writes: none (the routing manifest did not verify)".to_string(),
    };

    let routing_line = match auth_status_routing_problem(paths, config_doc, now) {
        Ok(description) => format!("ready ({description})"),
        Err((description, reason)) => {
            unavailable.push(reason);
            format!("unavailable ({description})")
        }
    };

    let mut text = String::new();
    text.push_str(
        "github.com (answered by the AFT gh shim from local state; the real gh was not run)\n",
    );
    text.push_str(&format!(
        "  Repository: {}\n",
        repository.as_deref().unwrap_or("none")
    ));
    text.push_str(&format!("  {identity_line}\n"));
    text.push_str(&format!("  - Routing manifest: {manifest_line}\n"));
    text.push_str(&format!("  - Governed routing: {routing_line}\n"));
    // Reads are classified mechanical and handed to the real `gh` unchanged
    // (see `classify` and `is_unbound_safe`), so they use whatever login the
    // operator's `gh` holds; the bot identity applies to governed writes only.
    text.push_str(
        "  - Reads: run by the real gh under the operator's own gh login, not the bot identity\n",
    );
    for reason in &unavailable {
        text.push_str(&format!("Governed writes unavailable: {reason}\n"));
    }
    AuthStatusAnswer::Report {
        text,
        exit_code: i32::from(!unavailable.is_empty()),
    }
}

/// Whether governed routing is ready, judged from the configured connection
/// file and the last recorded rung without probing the daemon. `Ok` carries
/// a description of the ready state; `Err` carries a description and the
/// reason governed writes are unavailable. Neither names a token, an
/// installation, or the connection file's contents.
fn auth_status_routing_problem(
    paths: &StatePaths,
    config_doc: Option<&str>,
    now: u64,
) -> Result<String, (String, String)> {
    let Some(connection_file) = connection_file_from_config_doc(config_doc.unwrap_or("")) else {
        return Err((
            "no governance connection file is configured".to_string(),
            "governed routing is not configured: the user aft.jsonc names no subc.connection_file"
                .to_string(),
        ));
    };
    if !connection_file.is_file() {
        return Err((
            "the configured governance connection file is missing".to_string(),
            "governed routing is unavailable: the configured governance connection file does not exist, so the governance daemon is not running"
                .to_string(),
        ));
    }
    let Some(record) = load_rung_record(paths) else {
        return Err((
            "connection file present; no rung recorded yet".to_string(),
            "governed routing has not been confirmed on this machine yet (no rung recorded); the next governed write probes the governance daemon"
                .to_string(),
        ));
    };
    let age = now.saturating_sub(record.as_of_unix_secs);
    if record.rung == Rung::R3 {
        return Ok(format!(
            "last rung R3, recorded {age}s ago; connection file present"
        ));
    }
    // Name the determination inputs that kept the rung below R3. Only input
    // names and diagnostic words are printed: one input's value can name
    // where an ambient credential was found, so its value is left out.
    let causes = record
        .inputs
        .iter()
        .filter(|(_, value)| !matches!(value.as_str(), "ready" | "absent"))
        .map(|(key, value)| match (key.as_str(), value.as_str()) {
            ("connection_file", diagnostic) => format!("connection_file {diagnostic}"),
            (key, _) => key.to_string(),
        })
        .collect::<Vec<_>>();
    let causes = if causes.is_empty() {
        "no recorded cause".to_string()
    } else {
        causes.join(", ")
    };
    let rung = record.rung.label();
    Err((
        format!("last rung {rung}: {causes}, recorded {age}s ago; connection file present"),
        format!(
            "governed routing is unavailable: the last rung recorded was {rung} ({causes}), not R3"
        ),
    ))
}

/// The third positional word (`add` in `gh repo deploy-key add key.pub`).
fn nested_action(args: &[OsString]) -> Option<String> {
    let mut skip_next = false;
    args.iter()
        .filter_map(|arg| arg.to_str())
        .filter(|value| {
            if std::mem::take(&mut skip_next) {
                return false;
            }
            if matches!(*value, "--repo" | "-R" | "--hostname" | "--config-dir") {
                skip_next = true;
                return false;
            }
            !value.starts_with('-')
        })
        .nth(2)
        .map(str::to_ascii_lowercase)
}

/// Whether a `gh api` call (arguments from `api` on) writes.
fn api_invocation_writes(args: &[OsString]) -> bool {
    let Some(shape) = api_request_shape(args) else {
        return true;
    };
    let method = shape.effective_method();
    if shape.path == "/graphql" {
        return !graphql_query_is_read_only(args);
    }
    if matches!(method.as_str(), "GET" | "HEAD") {
        return false;
    }
    true
}

/// Admit only inline documents consisting entirely of query operations.
/// External payloads and unknown argv forms cannot establish that the actual
/// document sent by gh is the one inspected here, so they fail closed.
fn graphql_query_is_read_only(args: &[OsString]) -> bool {
    let mut queries = Vec::new();
    let mut endpoint_seen = false;
    let mut index = 1;
    while index < args.len() {
        let Some(value) = args[index].to_str() else {
            return false;
        };
        index += 1;
        if value == "--input" || value.starts_with("--input=") {
            return false;
        }
        let field = if matches!(value, "-f" | "-F" | "--field" | "--raw-field") {
            let Some(field) = args.get(index).and_then(|arg| arg.to_str()) else {
                return false;
            };
            index += 1;
            Some(field)
        } else if let Some(field) = value
            .strip_prefix("--field=")
            .or_else(|| value.strip_prefix("--raw-field="))
        {
            Some(field)
        } else if value.len() > 2 && (value.starts_with("-f") || value.starts_with("-F")) {
            Some(&value[2..])
        } else {
            None
        };
        if let Some(field) = field {
            let Some((key, text)) = field.split_once('=') else {
                return false;
            };
            if key == "query" {
                // gh expands repository/branch placeholders in typed fields.
                // A branch name can contain GraphQL punctuation, so the bytes
                // inspected here would not necessarily be the document sent.
                let magic =
                    value == "--field" || value.starts_with("--field=") || value.starts_with("-F");
                if magic
                    && ["{owner}", "{repo}", "{branch}"]
                        .iter()
                        .any(|placeholder| text.contains(placeholder))
                {
                    return false;
                }
                queries.push(text);
            } else if key.starts_with("query[") {
                // gh's bracket syntax can replace a scalar with an array or
                // object. Do not let another field overwrite the inspected query.
                return false;
            }
        } else if matches!(value, "graphql" | "/graphql") && !endpoint_seen {
            endpoint_seen = true;
        } else if matches!(
            value,
            "--method"
                | "-X"
                | "--header"
                | "-H"
                | "--hostname"
                | "--cache"
                | "--jq"
                | "-q"
                | "--template"
                | "-t"
        ) {
            if args.get(index).and_then(|arg| arg.to_str()).is_none() {
                return false;
            }
            index += 1;
        } else if matches!(
            value,
            "--paginate" | "--slurp" | "--silent" | "--include" | "-i" | "--verbose"
        ) || [
            "--method=",
            "-X",
            "--header=",
            "-H",
            "--hostname=",
            "--cache=",
            "--jq=",
            "-q",
            "--template=",
            "-t",
        ]
        .iter()
        .any(|prefix| {
            value
                .strip_prefix(prefix)
                .is_some_and(|rest| !rest.is_empty())
        }) {
            // These flags do not supply or replace the request document.
        } else {
            return false;
        }
    }
    endpoint_seen
        && !queries.is_empty()
        && queries
            .iter()
            .all(|query| graphql_document_is_read_only(query))
}

fn graphql_document_is_read_only(document: &str) -> bool {
    graphql_document_shape(document).read_only
}

#[derive(Default)]
struct GraphqlDocumentShape {
    read_only: bool,
    has_mutation: bool,
}

/// Delegated reads use the operator's credentials, so inspect the complete
/// syntax tree rather than guessing operation boundaries from delimiters.
/// Parser recovery trees cannot authorize reads. GitHub still validates fields
/// against its schema; only queries and fragments may cross this boundary.
fn graphql_document_shape(document: &str) -> GraphqlDocumentShape {
    use apollo_parser::{cst::Definition, Parser};

    let parsed = Parser::new(document).parse();
    if parsed.errors().next().is_some() {
        return GraphqlDocumentShape::default();
    }

    let mut shape = GraphqlDocumentShape {
        read_only: true,
        has_mutation: false,
    };
    let mut has_operation = false;
    for definition in parsed.document().definitions() {
        match definition {
            Definition::OperationDefinition(operation) => {
                has_operation = true;
                if let Some(kind) = operation.operation_type() {
                    shape.read_only &= kind.query_token().is_some();
                    shape.has_mutation |= kind.mutation_token().is_some();
                }
            }
            Definition::FragmentDefinition(_) => {}
            _ => shape.read_only = false,
        }
    }
    shape.read_only &= has_operation;
    shape
}

/// What a write acts on (see `WriteTarget`), from the same resolver the
/// governed path uses: `--repo`, a URL or repository positional, `GH_REPO`,
/// then the working directory's origin.
fn write_target(args: &[OsString], target: &TargetRepository, cwd: &Path) -> WriteTarget {
    if let Some((verb, subcommand, head_index)) = command_head(args) {
        if verb == "api" {
            // `{owner}/{repo}` placeholders are filled by upstream `gh` from
            // `GH_REPO` or the origin, so those endpoints resolve below.
            if let Some(shape) = api_request_shape(&args[head_index..]) {
                if !shape.path.starts_with("/repos/") {
                    return WriteTarget::NotARepository {
                        description: format!(
                            "calls {}, an endpoint outside /repos/<owner>/<repo>",
                            shape.path
                        ),
                        named: None,
                    };
                }
            }
        }
        let tuple = verb_tuple(verb, subcommand);
        if ACCOUNT_WRITE_TUPLES.contains(&tuple.as_str()) {
            // Only a positional names the repository here; `GH_REPO` and the
            // origin say nothing about a repository being created or forked.
            let named = target.url.clone();
            return WriteTarget::NotARepository {
                description: account_write_description(&tuple, named.as_deref()),
                named,
            };
        }
    }
    match target.named() {
        Some(named) => match canonical_repository_key(named) {
            Some(repository) => WriteTarget::Repository(repository),
            None => WriteTarget::Undetermined(format!(
                "`{named}` is not a github.com owner/name repository"
            )),
        },
        None => match repository_key_from_origin(&project_root_for(cwd)) {
            Some(repository) => WriteTarget::Repository(repository),
            None => WriteTarget::Undetermined(
                "no --repo, repository URL or GH_REPO names one, and the working directory has no github.com origin remote"
                    .to_string(),
            ),
        },
    }
}

fn account_write_description(tuple: &str, named: Option<&str>) -> String {
    match (tuple, named) {
        ("repo create", Some(named)) => format!("creates the new repository {named}"),
        ("repo create", None) => "creates a new repository".to_string(),
        ("repo fork", Some(named)) => format!("forks {named} into the caller's account"),
        ("repo fork", None) => "creates a fork in the caller's account".to_string(),
        _ if tuple.starts_with("gist ") => "acts on the caller's gists".to_string(),
        _ if tuple.starts_with("project ") => {
            "acts on a project owned by a user or organization".to_string()
        }
        _ => "changes the caller's account keys".to_string(),
    }
}

/// Refusal text for an undeclared invocation.
///
/// Native commands name their verb; API commands name their method and endpoint
/// or GraphQL operation kind, so an undeclared mutation does not look like a
/// refusal of all API reads. Output flags are not the classification boundary.
fn unclassified_refusal_text(args: &[OsString], manifest_version: u64) -> String {
    let subject = match invocation_verb(args) {
        Some(verb) if verb == "api" => api_refusal_subject(args),
        Some(verb) => format!("verb \"{verb}\""),
        // A non-UTF-8 argument vector has no verb that can be quoted back.
        None => "this invocation".to_string(),
    };
    format!(
        "{subject} is not declared in manifest {manifest_version} (output flags such as --json/-q are not the reason); GH_SHIM_BYPASS does not apply to undeclared invocations - this verb needs a manifest declaration"
    )
}

fn api_refusal_subject(args: &[OsString]) -> String {
    let Some((_, _, index)) = command_head(args) else {
        return "api (uninspectable endpoint)".into();
    };
    let args = &args[index..];
    let Some(shape) = api_request_shape(args) else {
        return "api (uninspectable endpoint)".into();
    };
    if shape.path != "/graphql" {
        return format!("api {} {}", shape.effective_method(), shape.path);
    }
    let mutation = args.iter().filter_map(|arg| arg.to_str()).any(|arg| {
        let field = arg
            .strip_prefix("--field=")
            .or_else(|| arg.strip_prefix("--raw-field="))
            .or_else(|| arg.strip_prefix("-f"))
            .or_else(|| arg.strip_prefix("-F"))
            .unwrap_or(arg);
        let Some(document) = field.strip_prefix("query=") else {
            return false;
        };
        graphql_document_shape(document).has_mutation
    });
    format!(
        "api graphql ({})",
        if mutation {
            "mutation"
        } else {
            "uninspectable query"
        }
    )
}

/// The verb tuple `classify` reads out of an argument vector: two words for a
/// verb with a subcommand (`issue create`), one for a bare verb (`api`).
fn invocation_verb(args: &[OsString]) -> Option<String> {
    let (verb, subcommand, _) = command_head(args)?;
    Some(verb_tuple(verb, subcommand))
}

fn verb_tuple(verb: String, subcommand: Option<String>) -> String {
    match subcommand {
        Some(subcommand) => format!("{verb} {subcommand}"),
        None => verb,
    }
}

fn refuse_governed_canonicalization(error: &CanonicalizeError) -> i32 {
    refuse(error.code, &error.text)
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
        RouteOutcome::ResultStderr(output) => {
            eprint!("{output}");
            0
        }
        RouteOutcome::StateAppliedCommentFailed(output) => {
            print!("{output}");
            UPSTREAM_FAILURE_EXIT_STATUS
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
            refuse_governance_unavailable(paths, agent_binding, now, GOVERNANCE_UNAVAILABLE_TEXT)
        }
        RouteOutcome::GovernanceUnavailableTimedOut { stage, elapsed_ms } => {
            let text = if stage == ProbeStage::Connect {
                GOVERNANCE_UNAVAILABLE_TEXT.to_string()
            } else {
                governance_probe_timeout_text(elapsed_ms, stage)
            };
            refuse_governance_unavailable(paths, agent_binding, now, &text)
        }
        RouteOutcome::OutcomeUnknown { elapsed_ms } => {
            refuse_outcome_unknown(paths, agent_binding, now, elapsed_ms)
        }
        RouteOutcome::Unavailable(message) => refuse(RefusalCode::SeamUnavailable, &message),
        RouteOutcome::NoAgentSession => refuse(RefusalCode::UnboundIdentity, NO_AGENT_SESSION_TEXT),
        RouteOutcome::RelayRefusal { text, .. } => refuse(RefusalCode::SeamRefusal, &text),
        RouteOutcome::OutcomeUndetermined(text) => {
            refuse_outcome_unknown_with_text(paths, agent_binding, now, &text)
        }
    }
}

/// Refusal text for a governed write from a command that has no agent session
/// ticket (for example, a terminal the operator opened by hand).
const NO_AGENT_SESSION_TEXT: &str =
    "no agent session is attached to this command; bot speech must come from an agent's own session";

fn seam_refusal_text(code: &str) -> String {
    format!("governance seam refused the action: {code}")
}

fn is_reserved_self_report(args: &[OsString]) -> bool {
    args.first()
        .and_then(|arg| arg.to_str())
        .is_some_and(|arg| RESERVED_SELF_REPORT.contains(&arg))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProbeStage {
    Connect,
    CatalogList,
    OpenRoute,
    Request,
}

impl ProbeStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::CatalogList => "catalog_list",
            Self::OpenRoute => "open_route",
            Self::Request => "request",
        }
    }
}

impl std::fmt::Display for ProbeStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LastProbeReport {
    stage: String,
    elapsed_ms: u64,
    outcome: String,
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
    manifests_dir: PathBuf,
    last_probe: PathBuf,
}

impl StatePaths {
    fn from_process() -> Self {
        Self::from_root(gh_shim_state_dir_from(
            crate::environment::non_empty_os_var(GH_SHIM_STATE_DIR_ENV).as_deref(),
            crate::environment::non_empty_os_var("XDG_STATE_HOME").as_deref(),
            crate::environment::non_empty_os_var("HOME").as_deref(),
        ))
    }

    fn from_root(root: PathBuf) -> Self {
        crate::private_storage::tighten_root(&root);
        Self {
            manifest: root.join("gh-routing-manifest.json"),
            rung: root.join("rung-cache.json"),
            bypass_audit: root.join("operator-bypass.jsonl"),
            unexpected_gh_route_advertisers: root.join("unexpected-gh-route-advertisers.json"),
            seam_state: root.join("seam-state.json"),
            last_valid_manifest: root.join("last-valid-manifest.json"),
            version_high_water: root.join("manifest-version-high-water.json"),
            numeric_ids: root.join("numeric-ids.json"),
            manifests_dir: root.join("manifests"),
            last_probe: root.join("last-probe.json"),
            root,
        }
    }
}

/// The gh-shim state files this process's environment resolves to, for the
/// test-gate check that none of them lands in the operator's real state home.
#[cfg(test)]
pub(crate) fn process_state_files_for_test() -> Vec<PathBuf> {
    let paths = StatePaths::from_process();
    vec![
        paths.rung,
        paths.seam_state,
        paths.last_probe,
        paths.last_valid_manifest,
        paths.root,
    ]
}

fn write_last_probe_silently(paths: &StatePaths, probe: &LastProbeReport) {
    let Ok(bytes) = serde_json::to_vec(probe) else {
        return;
    };
    let _ = crate::private_storage::open_root(&paths.root);
    let temporary = paths.last_probe.with_extension("tmp");
    if crate::private_storage::write(&temporary, bytes).is_ok() {
        let _ = fs::rename(temporary, &paths.last_probe);
    }
}

fn read_last_probe(paths: &StatePaths) -> Option<LastProbeReport> {
    serde_json::from_slice(&fs::read(&paths.last_probe).ok()?).ok()
}

#[cfg(all(test, unix))]
pub(crate) fn write_storage_permission_fixture(root: &Path) {
    let paths = StatePaths::from_root(root.to_path_buf());
    write_last_probe_silently(
        &paths,
        &LastProbeReport {
            stage: "connect".to_string(),
            elapsed_ms: 1,
            outcome: "ok".to_string(),
        },
    );
    assert!(paths.last_probe.is_file());
}

/// Resolve the one process-state directory used by every gh-shim reader and
/// writer. The dedicated absolute override is for embedding callers and tests;
/// otherwise the ladder is XDG state-home, then `$HOME/.local/state`.
///
/// This is a deliberate divergence from the daemon's storage ladder, and it
/// must stay one: the shim is not the supervised module. It runs inside the
/// agent's child process with the operator's environment, and every placed
/// artifact of the governance protocol - the signed routing manifest, the
/// version high-water that refuses rollbacks, the rung cache, the bypass
/// audit - lives at this path on every governed seat, written there by the
/// activation ceremony. Moving the rung silently would start every seat with
/// an empty state directory: no manifest reads as "unmanifested", which is
/// transparent passthrough under the operator's own credentials, so bot
/// speech would post as the operator fleet-wide with nothing refusing.
fn gh_shim_state_dir_from(
    dedicated_override: Option<&OsStr>,
    xdg_state_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> PathBuf {
    if let Some(path) = dedicated_override
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return path;
    }
    xdg_state_home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".local/state"))
        })
        .unwrap_or_else(std::env::temp_dir)
        .join("cortexkit")
        .join("aft")
        .join("gh-shim")
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
    #[serde(default)]
    last_reachable_unix_secs: Option<u64>,
}

#[derive(Clone, Debug)]
struct RungRecordProvenance {
    image_path: String,
    version: String,
    repo_key: String,
}

impl RungRecordProvenance {
    fn for_target(target: &TargetRepository, cwd: &Path) -> Self {
        Self {
            image_path: executing_image().to_string_lossy().into_owned(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            repo_key: target
                .repository_key(cwd)
                .unwrap_or_else(|| "unresolved (no GitHub origin)".to_string()),
        }
    }
}

impl RungRecord {
    fn fresh_at(&self, now: u64) -> bool {
        now.saturating_sub(self.as_of_unix_secs) < DISCOVERY_CACHE_TTL.as_secs()
    }

    fn recently_reachable(&self, now: u64) -> bool {
        let reachable_at = self
            .last_reachable_unix_secs
            .unwrap_or(self.as_of_unix_secs);
        now.saturating_sub(reachable_at) < RECENTLY_REACHABLE_WINDOW.as_secs()
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
    refusal_detail: Option<String>,
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
                last_reachable_unix_secs: None,
            },
            operator_disabled: reason == R1Reason::DisabledByConfig,
            refusal_detail: None,
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
                last_reachable_unix_secs: None,
            },
            operator_disabled: false,
            refusal_detail: None,
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
                last_reachable_unix_secs: Some(now),
            },
            operator_disabled: false,
            refusal_detail: None,
        }
    }

    fn cached(record: RungRecord) -> Self {
        Self {
            record,
            operator_disabled: false,
            refusal_detail: None,
        }
    }
}

#[derive(Debug)]
enum GovernanceDisposition {
    Delegate,
    Ready,
    Unavailable(AgentBinding),
    Unclassified { manifest_version: u64 },
    Destructive,
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
        Classification::Destructive => GovernanceDisposition::Destructive,
        Classification::Mechanical => GovernanceDisposition::Delegate,
    }
}

/// The binding that decides how a classified command is dispatched.
///
/// Speech belongs to the repository it is aimed at: a bound target routes as
/// its own bot, and an unbound target is not governed even from inside a bound
/// checkout. Every other write also refuses when only the checkout is bound
/// (see `target_or_checkout_binding`).
fn governing_binding(
    classification: &Classification,
    manifest: &Manifest,
    target: &TargetRepository,
    cwd: &Path,
) -> Option<AgentBinding> {
    match classification {
        Classification::Governed { .. } => resolved_agent_binding(manifest, target, cwd),
        _ => target_or_checkout_binding(manifest, target, cwd),
    }
}

fn non_r3_governance_disposition(
    cwd: &Path,
    target: &TargetRepository,
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
    if matches!(classification, Classification::Destructive) {
        return GovernanceDisposition::Destructive;
    }

    // Binding resolution runs `git` to inspect the origin. Classify first so
    // unmanifested public repositories keep the R1 fast path for mechanical
    // reads; only a verb that could refuse pays the subprocess latency.
    let agent_binding = governing_binding(&classification, manifest, target, cwd);
    structural_governance_disposition(
        determination,
        &classification,
        agent_binding,
        manifest.manifest_version,
    )
}

fn determine_rung(
    paths: &StatePaths,
    target: &TargetRepository,
    cwd: &Path,
    now: u64,
) -> RungDetermination {
    // The budget starts before the config read and connection-file stat. This
    // keeps a slow filesystem from silently extending discovery beyond the
    // per-stage budget.
    let deadline = std::time::Instant::now() + DISCOVERY_BUDGET;
    let config_doc = read_user_config_doc();
    determine_rung_for_target(paths, target, cwd, now, deadline, config_doc.as_deref())
}

/// Rung determination for a command that names no repository of its own.
#[cfg(test)]
fn determine_rung_from_doc(
    paths: &StatePaths,
    cwd: &Path,
    now: u64,
    deadline: std::time::Instant,
    config_doc: Option<&str>,
) -> RungDetermination {
    determine_rung_for_target(
        paths,
        &TargetRepository::default(),
        cwd,
        now,
        deadline,
        config_doc,
    )
}

/// Pure rung determination over the user config document. `config_doc` is the
/// raw user-tier `aft.jsonc` text (already read by the caller); `None` means the
/// config file was absent or unreadable. Splitting the config read from the
/// decision keeps the disabled short-circuit testable without mutating process
/// env (which races under the parallel test runner).
///
/// Governance is needed whenever the target or the checkout is bound, so the
/// binding that gates discovery is `target_or_checkout_binding`.
fn determine_rung_for_target(
    paths: &StatePaths,
    target: &TargetRepository,
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

    let provenance = RungRecordProvenance::for_target(target, cwd);
    // The state directory is shared by every repository for this user. A rung
    // determined for another target (including a missing binding) says nothing
    // about this target's governance health, even during timeout fallback.
    // Legacy or unresolved provenance cannot establish a repository match.
    let cached = load_rung_record(paths).filter(|record| {
        canonical_repository_key(&provenance.repo_key).is_some()
            && record.recorded_by_repo_key.as_deref() == Some(provenance.repo_key.as_str())
    });
    if std::time::Instant::now() >= deadline {
        let budget_ms = DISCOVERY_BUDGET.as_millis();
        let stage = ProbeStage::Connect;
        let probe = LastProbeReport {
            stage: stage.as_str().to_string(),
            elapsed_ms: budget_ms as u64,
            outcome: "timed_out".to_string(),
        };
        write_last_probe_silently(paths, &probe);
        if let Some(record) = cached.as_ref().filter(|record| {
            record.rung == Rung::R3 && (record.fresh_at(now) || record.recently_reachable(now))
        }) {
            return RungDetermination::cached(record.clone());
        }
        let mut determination = cached
            .filter(|record| record.fresh_at(now))
            .map(RungDetermination::cached)
            .unwrap_or_else(|| RungDetermination::r1(now, R1Reason::DiscoveryBudgetExhausted));
        if determination.record.rung == Rung::R1 {
            determination.refusal_detail =
                Some(governance_probe_timeout_text(budget_ms as u64, stage));
        }
        return determination;
    }
    if let Some(record) = cached.as_ref().filter(|record| record.fresh_at(now)) {
        if record.rung != Rung::R3
            || resolve_manifest(paths, now)
                .manifest()
                .and_then(|manifest| target_or_checkout_binding(manifest, target, cwd))
                .is_some()
        {
            return RungDetermination::cached(record.clone());
        }
    }

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
    let Some(agent_binding) = target_or_checkout_binding(&manifest, target, cwd) else {
        let determination = RungDetermination::r2(
            now,
            R2Reason::AgentBindingUnavailable,
            Some(manifest.manifest_version),
            &provenance,
        );
        write_rung_record_silently(paths, &determination.record);
        return determination;
    };

    let discovery = probe_governance_with_retry(
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
                    determination.record.last_reachable_unix_secs = Some(now);
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
            let mut determination =
                RungDetermination::r2(now, R2Reason::CatalogGhRouteAbsent, None, &provenance);
            determination.refusal_detail = Some(relay_client::RELAY_UNSERVED_TEXT.to_string());
            determination
        }
        // Keep the holder-unbound status diagnostic distinct from an absent
        // repository binding. Dispatch no longer consumes either reason.
        ProbeResult::Unbound => {
            RungDetermination::r2(now, R2Reason::GhRouteHolderUnbound, None, &provenance)
        }
        ProbeResult::TimedOut { stage, .. } => {
            if let Some(record) = cached.as_ref().filter(|record| {
                record.rung == Rung::R3 && (record.fresh_at(now) || record.recently_reachable(now))
            }) {
                RungDetermination::cached(record.clone())
            } else if stage == ProbeStage::Connect {
                RungDetermination::r2(now, R2Reason::DaemonUnreachable, None, &provenance)
            } else {
                let budget_ms = DISCOVERY_BUDGET.as_millis();
                let refusal_text = governance_probe_timeout_text(budget_ms as u64, stage);
                let mut determination = cached
                    .filter(|record| record.fresh_at(now))
                    .map(RungDetermination::cached)
                    .unwrap_or_else(|| {
                        RungDetermination::r1(now, R1Reason::DiscoveryBudgetExhausted)
                    });
                if determination.record.rung == Rung::R1 {
                    determination.refusal_detail = Some(refusal_text);
                }
                determination
            }
        }
    };

    if determination.record.rung != Rung::R1 {
        write_rung_record_silently(paths, &determination.record);
    }
    determination
}

pub fn configured_connection_file() -> Option<PathBuf> {
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    configured_connection_file_from(xdg_config_home.as_deref(), home.as_deref())
}

fn configured_connection_file_from(
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<PathBuf> {
    // This reader becomes `subc_transport::connection_file::discover(explicit)`
    // when the transport API reaches AFT. That call replaces these ordered rungs:
    // explicit (exclusive); non-empty SUBC_CONNECTION_FILE (exclusive); non-empty
    // XDG_RUNTIME_DIR/subc-connection.json; non-empty
    // HOME/.local/share/cortexkit/run/subc-connection.json; user-scoped temp file.
    // Last re-derived 2026-09-06 against subconscious
    // d5e09914b0791a66f2a5a00a9bb3422860ade95e: compare `(rung, guard)` pairs with
    // `subc-transport/src/connection_file.rs::discovery_candidates_with_environment`
    // and resolve `CONNECTION_FILE_NAME` and `PROD_CONNECTION_RELATIVE_PATH`.
    // Until the call lands here, only trusted user config can provide the explicit
    // path; invalid or unreadable paths resolve to `None` for the rung classifier.
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

/// Read the effective `github.enabled && github.shim` gate from user config.
/// The deprecated `gh_shim.enabled` alias remains a fallback for one minor.
fn gh_shim_enabled_from_config_doc(doc: &str) -> Option<bool> {
    let value: Value = serde_json::from_str(&crate::jsonc::strip_jsonc(doc)).ok()?;
    let github = value.get("github");
    let master = github
        .and_then(|config| config.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let shim = github
        .and_then(|config| config.get("shim"))
        .and_then(Value::as_bool)
        .or_else(|| {
            value
                .get("gh_shim")
                .and_then(|config| config.get("enabled"))
                .and_then(Value::as_bool)
        })
        .unwrap_or(true);
    Some(master && shim)
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
    let _ = crate::private_storage::open_root(&paths.root);
    let temporary = paths.root.join("rung-cache.json.tmp");
    if crate::private_storage::write(&temporary, bytes).is_ok() {
        let _ = fs::rename(temporary, &paths.rung);
    }
}

#[derive(Debug)]
enum ProbeResult {
    Ready { module_id: String },
    Unreachable,
    NoRoute,
    Unbound,
    TimedOut { stage: ProbeStage },
}

/// Run the discovery probe once, and on a deadline expiry retry the whole
/// probe once after a short backoff before refusing. A loaded host can blow
/// the per-stage budget before the daemon answers; the daemon is usually
/// reachable on the second attempt. A real connection refusal (not a deadline
/// expiry) is returned immediately without a retry, because retrying cannot
/// turn a refused connection into a reachable daemon.
fn probe_governance_with_retry(
    paths: &StatePaths,
    connection_file: &Path,
    cwd: &Path,
    deadline: std::time::Instant,
    agent_id: &str,
) -> ProbeResult {
    let first = probe_governance(paths, connection_file, cwd, deadline, agent_id);
    if !matches!(first, ProbeResult::TimedOut { .. }) {
        return first;
    }
    std::thread::sleep(DISCOVERY_RETRY_BACKOFF);
    // The retry gets a fresh per-stage budget: the original deadline has
    // already elapsed after the first attempt plus the backoff, so reusing it
    // would make the retry time out at the connect stage before it dials.
    let retry_deadline = std::time::Instant::now() + DISCOVERY_BUDGET;
    probe_governance(paths, connection_file, cwd, retry_deadline, agent_id)
}

fn probe_governance(
    paths: &StatePaths,
    connection_file: &Path,
    cwd: &Path,
    deadline: std::time::Instant,
    agent_id: &str,
) -> ProbeResult {
    let start = std::time::Instant::now();
    let remaining = deadline.saturating_duration_since(start);
    if remaining.is_zero() {
        let probe = LastProbeReport {
            stage: ProbeStage::Connect.as_str().to_string(),
            elapsed_ms: DISCOVERY_BUDGET.as_millis() as u64,
            outcome: "timed_out".to_string(),
        };
        write_last_probe_silently(paths, &probe);
        return ProbeResult::TimedOut {
            stage: ProbeStage::Connect,
        };
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
        let probe = LastProbeReport {
            stage: ProbeStage::Connect.as_str().to_string(),
            elapsed_ms: start.elapsed().as_millis() as u64,
            outcome: "unreachable".to_string(),
        };
        write_last_probe_silently(paths, &probe);
        return ProbeResult::Unreachable;
    };

    let current_stage = std::sync::Arc::new(std::sync::Mutex::new(ProbeStage::Connect));
    let stage_handle = std::sync::Arc::clone(&current_stage);

    // `tokio::time::timeout` creates its timer immediately. Building that
    // future as a `block_on` argument happens before the runtime enters its
    // context, so the timer's reactor lookup panics in this synchronous CLI.
    // Construct it from inside the entered future instead.
    let result = runtime.block_on(async move {
        tokio::time::timeout(remaining, async move {
            let options = ConsumerOptions {
                call_timeout: remaining,
                ..ConsumerOptions::default()
            };
            let consumer = SubcConsumer::connect(&connection_file, options)
                .await
                .map_err(|_| ProbeResult::Unreachable)?;

            *stage_handle.lock().unwrap() = ProbeStage::CatalogList;
            let catalog = consumer
                .catalog_list()
                .await
                .map_err(|_| ProbeResult::Unreachable)?;
            let holder = route_holder(&catalog.modules);
            record_unexpected_gh_route_advertisers(&record_paths, &holder.unexpected_advertisers);
            // Governed writes now travel through the AFT daemon's relay, so
            // discovery asks whether `aft` serves it rather than who holds
            // `gh.route`. The management route opened below is the one each
            // governed write uses.
            let Some(module_id) = relay_client::relay_holder(&catalog.modules) else {
                return Err(ProbeResult::NoRoute);
            };

            *stage_handle.lock().unwrap() = ProbeStage::OpenRoute;
            // `BindIdentity::new` sends no registered project id; the shim
            // never resolved one before the field existed.
            let identity = BindIdentity::new(
                project_root.to_string_lossy().into_owned(),
                "aft-gh-shim",
                gh_session_id(&agent_id),
            );
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
    });

    let final_stage = *current_stage.lock().unwrap();
    let elapsed_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(Ok(module_id)) => {
            let probe = LastProbeReport {
                stage: final_stage.as_str().to_string(),
                elapsed_ms,
                outcome: "ready".to_string(),
            };
            write_last_probe_silently(paths, &probe);
            ProbeResult::Ready { module_id }
        }
        Ok(Err(ProbeResult::Unreachable)) => {
            if std::time::Instant::now() >= deadline {
                let probe = LastProbeReport {
                    stage: final_stage.as_str().to_string(),
                    elapsed_ms: elapsed_ms.max(DISCOVERY_BUDGET.as_millis() as u64),
                    outcome: "timed_out".to_string(),
                };
                write_last_probe_silently(paths, &probe);
                ProbeResult::TimedOut { stage: final_stage }
            } else {
                let probe = LastProbeReport {
                    stage: final_stage.as_str().to_string(),
                    elapsed_ms,
                    outcome: "unreachable".to_string(),
                };
                write_last_probe_silently(paths, &probe);
                ProbeResult::Unreachable
            }
        }
        Ok(Err(ProbeResult::NoRoute)) => {
            let probe = LastProbeReport {
                stage: final_stage.as_str().to_string(),
                elapsed_ms,
                outcome: "no_route".to_string(),
            };
            write_last_probe_silently(paths, &probe);
            ProbeResult::NoRoute
        }
        Ok(Err(ProbeResult::Unbound)) => {
            if std::time::Instant::now() >= deadline {
                let probe = LastProbeReport {
                    stage: final_stage.as_str().to_string(),
                    elapsed_ms: elapsed_ms.max(DISCOVERY_BUDGET.as_millis() as u64),
                    outcome: "timed_out".to_string(),
                };
                write_last_probe_silently(paths, &probe);
                ProbeResult::TimedOut { stage: final_stage }
            } else {
                let probe = LastProbeReport {
                    stage: final_stage.as_str().to_string(),
                    elapsed_ms,
                    outcome: "unbound".to_string(),
                };
                write_last_probe_silently(paths, &probe);
                ProbeResult::Unbound
            }
        }
        Ok(Err(other)) => other,
        Err(_) => {
            let probe = LastProbeReport {
                stage: final_stage.as_str().to_string(),
                elapsed_ms: elapsed_ms.max(DISCOVERY_BUDGET.as_millis() as u64),
                outcome: "timed_out".to_string(),
            };
            write_last_probe_silently(paths, &probe);
            ProbeResult::TimedOut { stage: final_stage }
        }
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

/// The binding of the repository the command targets (see `TargetRepository`).
fn resolved_agent_binding(
    manifest: &Manifest,
    target: &TargetRepository,
    cwd: &Path,
) -> Option<AgentBinding> {
    let repo = target.repository_key(cwd)?;
    manifest
        .bindings
        .get(&repo)
        .cloned()
        .map(|agent_id| AgentBinding { repo, agent_id })
}

/// The binding of the checkout the working directory sits in, whatever a
/// command might name.
fn checkout_agent_binding(manifest: &Manifest, cwd: &Path) -> Option<AgentBinding> {
    resolved_agent_binding(manifest, &TargetRepository::default(), cwd)
}

/// The target's binding, else the checkout's.
///
/// Speech is governed by the target alone. This wider answer serves the
/// questions where a bound checkout matters too: whether governance must be
/// probed at all, and whether a non-speech write (admin, undeclared,
/// destructive) must refuse. An agent working in a governed checkout goes
/// through the audited operator bypass for administration whichever
/// repository it aims at.
fn target_or_checkout_binding(
    manifest: &Manifest,
    target: &TargetRepository,
    cwd: &Path,
) -> Option<AgentBinding> {
    resolved_agent_binding(manifest, target, cwd).or_else(|| {
        // With nothing named, the target already was the checkout.
        target
            .named()
            .and_then(|_| checkout_agent_binding(manifest, cwd))
    })
}

fn co_author_line(paths: &StatePaths) -> Option<String> {
    // Commits are made in the checkout, so the trailer names the checkout's
    // bot rather than any repository a `gh` command might target.
    let cwd = std::env::current_dir().ok()?;
    let manifest = load_manifest(paths, unix_seconds()).ok()?;
    let binding = checkout_agent_binding(&manifest, &cwd)?;
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
    if crate::private_storage::open_root(&paths.root).is_err() {
        return;
    }
    let temporary = paths.root.join("numeric-ids.json.tmp");
    if crate::private_storage::write(&temporary, bytes).is_ok() {
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
    if !crate::developer_tools::git_usable() {
        return None;
    }
    let output = crate::effective_path::new_command("git")
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

    for raw_pattern in &detectors.wrapper_config_dirs {
        let pattern = wrapper_config_dir_pattern_from_process(raw_pattern);
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
    let configured = crate::environment::non_empty_os_var("GH_CONFIG_DIR").map(PathBuf::from)?;
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

/// Expand a manifest `wrapper_config_dirs` pattern against the process env.
pub(crate) fn wrapper_config_dir_pattern_from_process(pattern: &str) -> String {
    let home = crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"))
        .map(PathBuf::from);
    let xdg_config_home =
        crate::environment::non_empty_os_var("XDG_CONFIG_HOME").map(PathBuf::from);
    expand_home_pattern(pattern, home.as_deref(), xdg_config_home.as_deref())
}

/// `~/.config/` in a manifest pattern names the user config home, so it
/// follows an absolute `XDG_CONFIG_HOME` the same way the user-tier
/// `aft.jsonc` lookup does; any other `~/` prefix expands against HOME.
fn expand_home_pattern(
    pattern: &str,
    home: Option<&Path>,
    xdg_config_home: Option<&Path>,
) -> String {
    if let Some(suffix) = pattern.strip_prefix("~/.config/") {
        if let Some(config_home) = xdg_config_home.filter(|path| path.is_absolute()) {
            return config_home.join(suffix).to_string_lossy().into_owned();
        }
    }
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
            if let Some(platform) = rule
                .platform
                .iter()
                .find(|platform| !matches!(platform.as_str(), "macos" | "linux"))
            {
                return Err(format!(
                    "api rule {} {} names unknown host platform {platform}",
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
            // A create names no target that exists yet, so a fields-only
            // declaration carries body fields and an empty target. Requiring
            // the form name keeps an empty target deliberate rather than a
            // truncated declaration that would admit an unparsed argv.
            let fields_only = is_fields_only(canonical);
            let incomplete = canonical.argv_forms.is_empty()
                || if fields_only {
                    canonical.body_fields.is_empty()
                } else {
                    canonical.target_fields.is_empty()
                };
            if incomplete {
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
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
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

struct VerifiedEnvelope {
    envelope: SignedManifest,
    trust_set: Vec<Option<ManifestTrustKey>>,
    verified: VerifiedManifest,
}

thread_local! {
    // Only cryptographic verification and parsing are memoized, by exact
    // envelope bytes and trust keys. Disk presence, rollback state, issue time,
    // and local state repair are checked on every resolution as before.
    static VERIFIED_ENVELOPE: std::cell::RefCell<Option<VerifiedEnvelope>> = const { std::cell::RefCell::new(None) };
}

fn verify_manifest_envelope(
    envelope: &SignedManifest,
    trust_set: &[Option<ManifestTrustKey>],
) -> Result<VerifiedManifest, ManifestProblem> {
    if let Some(verified) = VERIFIED_ENVELOPE.with(|cached| {
        cached
            .borrow()
            .as_ref()
            .filter(|cached| cached.envelope == *envelope && cached.trust_set == trust_set)
            .map(|cached| cached.verified.clone())
    }) {
        return Ok(verified);
    }
    let verified = verify_manifest_signature_with_provenance(envelope, trust_set)?;
    VERIFIED_ENVELOPE.with(|cached| {
        *cached.borrow_mut() = Some(VerifiedEnvelope {
            envelope: envelope.clone(),
            trust_set: trust_set.to_vec(),
            verified: verified.clone(),
        });
    });
    Ok(verified)
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
    } = verify_manifest_envelope(&envelope, trust_set)?;
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
    // Retain the exact verified bytes for later reproducibility checks. The
    // signature already verified above; the admission filter re-checks that the
    // version inside the verified payload matches the filing name. This never
    // affects activation.
    retain_manifest(paths, &envelope, manifest.manifest_version);
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
    #[cfg(test)]
    MANIFEST_WORK.with(|count| {
        let (verifications, writes) = count.get();
        count.set((verifications + 1, writes));
    });
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
#[derive(Clone, Copy, PartialEq, Eq)]
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

// The dev test key exists in debug images and in test builds of any profile
// (so `cargo test --release` compiles); it never enters a release trust set,
// so the release-profile lib tests that verify under it are debug-only below.
#[cfg(any(debug_assertions, test))]
const DEV_MANIFEST_KEY_ID: &str = "gh-routing-dev-test-key-v1";
#[cfg(any(debug_assertions, test))]
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
        Classification::Destructive => RegressedDisposition::Refuse {
            code: RefusalCode::DestructiveFlag,
            text: "destructive GitHub operations are not available through the shim".to_string(),
        },
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
    // Re-check the actual file, rather than remembering that a previous write
    // succeeded. Deletion, corruption, or another process's replacement still
    // repairs the last-valid record on the next accepted resolution.
    if fs::read(&paths.last_valid_manifest).is_ok_and(|existing| existing == bytes) {
        return;
    }
    #[cfg(test)]
    MANIFEST_WORK.with(|count| {
        let (verifications, writes) = count.get();
        count.set((verifications, writes + 1));
    });
    let _ = crate::private_storage::open_root(&paths.root);
    let temporary = paths.last_valid_manifest.with_extension("tmp");
    if crate::private_storage::write(&temporary, bytes).is_ok() {
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
    let _ = crate::private_storage::open_root(&paths.root);
    let temporary = paths.version_high_water.with_extension("tmp");
    if crate::private_storage::write(&temporary, bytes).is_ok() {
        let _ = fs::rename(temporary, &paths.version_high_water);
    }
}

/// One retained signed manifest: the exact verified payload bytes, the
/// signature, and the key id. This is byte-for-byte what was verified, never a
/// re-serialization, so a later reproducibility check (e.g. the signer needing
/// the v7 bytes) can read it back and re-verify it without trusting this
/// machine's memory of the manifest.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct RetainedManifest {
    manifest_bytes: String,
    signature: String,
    key_id: String,
}

/// Retain one accepted signed manifest on the consumer side.
///
/// On every successful activation the shim files the exact verified bytes so a
/// later reproducibility check can read them back. The file is named from the
/// version read INSIDE the verified payload bytes, never from a caller-supplied
/// label: a signature authenticates bytes, not a label, so a validly signed
/// superseded v9 must never be filed as v12.
///
/// ADMISSION FILTER (both must hold):
///   1. the envelope signature already verified against the compiled trust root
///      (the caller reuses that result — this function does not re-verify), and
///   2. the `manifest_version` parsed from inside the verified payload bytes
///      equals the version used in the filing name.
///
/// On mismatch the file is refused (logged once) and activation is unaffected.
/// Existing files are never overwritten: identical bytes are a no-op; different
/// bytes at the same name is a collision that must not silently replace
/// evidence, so it is refused with a loud log line.
fn retain_manifest(paths: &StatePaths, envelope: &SignedManifest, filing_version: u64) {
    // The version that names the file must come from inside the verified bytes,
    // not from any caller-supplied label. The envelope is already verified, so
    // this parse is over authenticated bytes.
    let inside_version = match serde_json::from_str::<Manifest>(&envelope.manifest_bytes) {
        Ok(manifest) => manifest.manifest_version,
        Err(_) => {
            eprintln!(
                "gh-shim: refusing to retain manifest: verified payload bytes failed to parse"
            );
            return;
        }
    };
    if inside_version != filing_version {
        eprintln!(
            "gh-shim: refusing to retain manifest: payload version {inside_version} does not match filing version {filing_version}"
        );
        return;
    }

    let digest = Sha256::digest(envelope.manifest_bytes.as_bytes());
    let digest_hex = format!("{digest:x}");
    let file_name = format!("v{inside_version}-{}.json", &digest_hex[..16]);
    let destination = paths.manifests_dir.join(&file_name);

    if destination.exists() {
        // Identical bytes are a no-op; different bytes at the same name is a
        // collision that must never silently replace evidence. The existing
        // file holds a RetainedManifest record, so compare its payload bytes.
        let existing_bytes = fs::read(&destination)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<RetainedManifest>(&bytes).ok())
            .map(|record| record.manifest_bytes);
        if existing_bytes.as_deref() == Some(envelope.manifest_bytes.as_str()) {
            return;
        }
        eprintln!(
            "gh-shim: refusing to retain manifest: collision at {} (different bytes at the same name)",
            destination.display()
        );
        return;
    }

    if crate::private_storage::open_dir(&paths.root, &paths.manifests_dir).is_err() {
        eprintln!("gh-shim: refusing to retain manifest: could not create manifests dir");
        return;
    }

    let record = RetainedManifest {
        manifest_bytes: envelope.manifest_bytes.clone(),
        signature: envelope.signature.clone(),
        key_id: envelope.key_id.clone(),
    };
    let Ok(bytes) = serde_json::to_vec(&record) else {
        eprintln!("gh-shim: refusing to retain manifest: serialization failed");
        return;
    };

    let temporary = paths.manifests_dir.join(format!(".{file_name}.tmp"));
    if crate::private_storage::write(&temporary, &bytes).is_err() {
        eprintln!("gh-shim: refusing to retain manifest: could not write temporary file");
        return;
    }
    if fs::rename(&temporary, &destination).is_err() {
        eprintln!("gh-shim: refusing to retain manifest: could not move into place");
        let _ = fs::remove_file(&temporary);
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
    Destructive,
}

fn is_reviewed_admin_tuple(manifest_version: u64, tuple: &str) -> bool {
    V1_ADMIN_TUPLES.contains(&tuple)
        || (manifest_version >= 9 && V9_ADMIN_TUPLES.contains(&tuple))
        || (manifest_version >= 10 && V10_ADMIN_TUPLES.contains(&tuple))
        || (manifest_version >= 11 && V11_ADMIN_TUPLES.contains(&tuple))
        || (manifest_version >= 13 && V13_ADMIN_TUPLES.contains(&tuple))
        || (manifest_version >= 15 && V15_ADMIN_TUPLES.contains(&tuple))
}

fn is_reviewed_governed_tuple(manifest_version: u64, tuple: &str) -> bool {
    V1_GOVERNED_TUPLES.contains(&tuple)
        || (manifest_version >= 12 && V12_GOVERNED_TUPLES.contains(&tuple))
        || (manifest_version >= 14 && V14_GOVERNED_TUPLES.contains(&tuple))
        || (manifest_version >= 16 && V16_GOVERNED_TUPLES.contains(&tuple))
}

/// True for a governed tuple whose label-only form the operator bypass may run
/// upstream. Only reached for a tuple the manifest already declares governed.
fn is_reviewed_operator_label_tuple(manifest_version: u64, tuple: &str) -> bool {
    manifest_version >= 14 && V14_OPERATOR_LABEL_TUPLES.contains(&tuple)
}

/// True for an admin-tier tuple that runs only in its operator row's narrow
/// shape (`pr edit` with labels only, `label create`), never as a plain
/// bypass. Only reached for a tuple the manifest already declares admin.
fn is_reviewed_operator_row_admin_tuple(manifest_version: u64, tuple: &str) -> bool {
    manifest_version >= 14 && V14_OPERATOR_ROW_ADMIN_TUPLES.contains(&tuple)
}

/// True for the one API rule that may be governed rather than admin: the v14
/// own-comment PATCH. Every other governed API rule stays undeclared, so a
/// signed rule alone cannot widen raw API writes into bot speech.
fn is_reviewed_governed_api_rule(manifest_version: u64, rule: &ApiRule) -> bool {
    manifest_version >= 14
        && rule.method.eq_ignore_ascii_case(OWN_COMMENT_PATCH_METHOD)
        && rule.path_glob == OWN_COMMENT_PATCH_PATH_GLOB
}

/// The own-comment PATCH is declared by an API rule, which carries no
/// canonicalization of its own (manifest canonicalization keys must name a
/// governed argv tuple). Its shape is fixed here instead: the comment id comes
/// from the endpoint path and the body is the only admitted field.
fn own_comment_patch_canonicalization() -> Canonicalization {
    Canonicalization {
        argv_forms: vec![BODY_ONLY_FORM.to_string()],
        target_fields: vec!["comment_id".to_string()],
        body_fields: vec!["body".to_string()],
    }
}

fn is_fields_only(canonical: &Canonicalization) -> bool {
    canonical
        .argv_forms
        .iter()
        .any(|form| form == FIELDS_ONLY_FORM)
}

fn is_target_and_state(canonical: &Canonicalization) -> bool {
    canonical
        .argv_forms
        .iter()
        .any(|form| form == TARGET_AND_STATE_FORM)
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
    let tuple = verb_tuple(verb, subcommand);
    if DESTRUCTIVE_TUPLES.contains(&tuple.as_str())
        || (tuple.starts_with("release ")
            && args.iter().any(|arg| {
                arg.to_str()
                    .is_some_and(|value| value.starts_with("--delete-"))
            }))
    {
        return Classification::Destructive;
    }
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
        Some(Tier::Admin)
            if is_reviewed_admin_tuple(manifest.manifest_version, &tuple)
                || is_reviewed_operator_row_admin_tuple(manifest.manifest_version, &tuple) =>
        {
            Classification::Admin { tuple }
        }
        Some(Tier::Governed) if is_reviewed_governed_tuple(manifest.manifest_version, &tuple) => {
            manifest
                .canonicalization
                .get(&tuple)
                .cloned()
                .map(|canonical| Classification::Governed { tuple, canonical })
                .unwrap_or(Classification::Unclassified)
        }
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
    let Some((method, path, has_fields)) = api_method_and_path(args) else {
        return Classification::Unclassified;
    };
    if path == "/graphql" {
        // Like a field-free REST GET, an inspected GraphQL read delegates to
        // the real gh unchanged under the operator's existing login. It never
        // mints an assertion or routes through the governed-write relay.
        return if graphql_query_is_read_only(args) {
            Classification::Mechanical
        } else {
            Classification::Unclassified
        };
    }
    let matches = manifest
        .api_rules
        .iter()
        .filter(|rule| {
            rule.method.eq_ignore_ascii_case(&method)
                && platform_matches(&rule.platform, platform)
                && glob::Pattern::new(&rule.path_glob).is_ok_and(|pattern| pattern.matches(&path))
        })
        .collect::<Vec<_>>();
    if matches.is_empty() && method.eq_ignore_ascii_case("GET") && !has_fields {
        // A field-free GET cannot write or assert an identity, so it remains a
        // mechanical read even when the manifest has no endpoint-specific rule.
        return Classification::Mechanical;
    }
    if matches.len() != 1 {
        return Classification::Unclassified;
    }
    let rule = matches[0];
    if rule.tier == Tier::Admin {
        return Classification::Admin {
            tuple: api_tuple(rule),
        };
    }
    // The own-comment PATCH is the one API write admitted as bot speech. It
    // crosses the field wall below because the shim parses its payload itself
    // and forwards only the single body field it recognized, rather than
    // handing the holder bytes it never read.
    if rule.tier == Tier::Governed && is_reviewed_governed_api_rule(manifest.manifest_version, rule)
    {
        return Classification::Governed {
            tuple: api_tuple(rule),
            canonical: own_comment_patch_canonicalization(),
        };
    }
    // Field payloads change request semantics independently of the endpoint.
    // Only ADMIN may cross this protection wall because it delegates under the
    // operator's own identity after writing the bypass audit; holder-bound
    // classifications must never sign a payload the shim has not parsed.
    if has_fields {
        return Classification::Unclassified;
    }
    match rule.tier {
        Tier::Mechanical => Classification::Mechanical,
        // Any other governed API rule stays undeclared until a parser accepts
        // and validates its exact argv forms. An id-addressed comment PATCH can
        // name any contributor's comment, unlike native `--edit-last`, which is
        // scoped to the caller; the reviewed own-comment rule above is admitted
        // only because the route holder checks that the comment's author is the
        // calling seat's bot before it writes.
        Tier::Governed => Classification::Unclassified,
        Tier::Admin => unreachable!("admin API rules return before field protection"),
    }
}

/// Stable name for an API rule, shared by admin bypass audit records and the
/// governed own-comment row: `api:<METHOD>:<path glob>`.
fn api_tuple(rule: &ApiRule) -> String {
    format!(
        "api:{}:{}",
        rule.method.to_ascii_uppercase(),
        rule.path_glob
    )
}

fn is_api_tuple(tuple: &str) -> bool {
    tuple.starts_with("api:")
}

fn api_method_and_path(args: &[OsString]) -> Option<(String, String, bool)> {
    let shape = api_request_shape(args)?;
    Some((
        shape.method.unwrap_or_else(|| "GET".to_string()),
        shape.path,
        shape.has_fields,
    ))
}

/// A `gh api` call as the shim reads it (arguments from `api` on).
struct ApiRequestShape {
    /// The method named by `--method`/`-X`, upper-cased; `None` when unnamed.
    method: Option<String>,
    /// The endpoint, with the leading slash upstream `gh` treats as implied.
    path: String,
    /// Whether any field or `--input` payload is given.
    has_fields: bool,
}

impl ApiRequestShape {
    /// The method upstream `gh` sends: the named one, else POST when a field
    /// or `--input` payload is given, else GET.
    fn effective_method(&self) -> String {
        match &self.method {
            Some(method) => method.clone(),
            None if self.has_fields => "POST".to_string(),
            None => "GET".to_string(),
        }
    }
}

fn api_request_shape(args: &[OsString]) -> Option<ApiRequestShape> {
    let mut method = None;
    let mut path = None;
    let mut has_fields = false;
    let mut index = 1;
    while index < args.len() {
        let value = args[index].to_str()?;
        if matches!(value, "--method" | "-X") {
            method = Some(args.get(index + 1)?.to_str()?.to_ascii_uppercase());
            index += 2;
            continue;
        }
        if let Some(method_value) = value
            .strip_prefix("--method=")
            .or_else(|| value.strip_prefix("-X="))
            .or_else(|| value.strip_prefix("-X").filter(|rest| !rest.is_empty()))
        {
            method = Some(method_value.to_ascii_uppercase());
            index += 1;
            continue;
        }
        if is_api_field_argument(value) {
            has_fields = true;
            if matches!(value, "--input" | "--raw-field" | "--field" | "-F" | "-f") {
                args.get(index + 1)?.to_str()?;
                index += 2;
            } else {
                index += 1;
            }
            continue;
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
    if path == "-" {
        return None;
    }
    // `gh api` accepts the endpoint with or without a leading slash
    // (`repos/o/r/...` and `/repos/o/r/...` are the same request), and the
    // slash-less spelling is the common one. Manifest globs are written with
    // the leading slash, so normalize here; otherwise the everyday form of a
    // declared admin endpoint reads as undeclared and refuses with the wrong
    // reason (v13 round trip, 2026-09-07).
    let path = if path.starts_with('/') || path.starts_with("http") {
        path
    } else {
        format!("/{path}")
    };
    Some(ApiRequestShape {
        method,
        path,
        has_fields,
    })
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

/// Pre-routing refusal produced while turning argv into a governed request.
/// Typed codes stay distinct from unclassified flag errors so callers can parse
/// the identifier rather than the prose.
#[derive(Debug)]
struct CanonicalizeError {
    code: RefusalCode,
    text: String,
}

impl CanonicalizeError {
    fn unclassified(text: impl Into<String>) -> Self {
        Self {
            code: RefusalCode::Unclassified,
            text: text.into(),
        }
    }

    fn typed(code: RefusalCode, text: impl Into<String>) -> Self {
        Self {
            code,
            text: text.into(),
        }
    }
}

impl From<String> for CanonicalizeError {
    fn from(text: String) -> Self {
        Self::unclassified(text)
    }
}

impl PartialEq<&str> for CanonicalizeError {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl std::fmt::Display for CanonicalizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl std::ops::Deref for CanonicalizeError {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

#[derive(Clone, Debug)]
struct GovernedRequest {
    action: String,
    target: Map<String, Value>,
    body: Map<String, Value>,
    repository: Option<String>,
    manifest_version: u64,
    edit_last: bool,
    author_scope: Option<String>,
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
            "issue comment" | "issue edit" | "issue reaction" | "issue close" | "issue reopen" => {
                GithubReadResourceKind::Issue
            }
            "pr comment" | "pr review" | "pr close" | "pr reopen" => {
                GithubReadResourceKind::PullRequest
            }
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
fn invalidate_successful_github_read_mutation_at(
    storage_root: &Path,
    mutation: Option<&GithubReadMutation>,
    outcome: &RouteOutcome,
) {
    if !matches!(
        outcome,
        RouteOutcome::Result(_)
            | RouteOutcome::ResultStderr(_)
            | RouteOutcome::StateAppliedCommentFailed(_)
    ) {
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
) -> Result<GovernedRequest, CanonicalizeError> {
    canonicalize_governed_from(
        args,
        tuple,
        canonical,
        manifest_version,
        &mut io::stdin().lock(),
    )
}

fn canonicalize_governed_from<R: Read>(
    args: &[OsString],
    tuple: &str,
    canonical: &Canonicalization,
    manifest_version: u64,
    stdin: &mut R,
) -> Result<GovernedRequest, CanonicalizeError> {
    let (_, _, head_index) = command_head(args)
        .ok_or_else(|| CanonicalizeError::unclassified("missing command head"))?;
    let subcommand_index = if tuple.starts_with("api ") {
        head_index
    } else {
        head_index + 1
    };
    if tuple == "pr create" {
        return canonicalize_pr_create_from(
            args,
            subcommand_index,
            canonical,
            manifest_version,
            stdin,
        );
    }
    let target_and_state = is_target_and_state(canonical);
    let fields_only = is_fields_only(canonical);
    let mut positional = Vec::new();
    let mut body = Map::new();
    let mut labels = Vec::new();
    let mut repeated_fields = BTreeMap::<String, Vec<String>>::new();
    let mut review_event = None;
    let mut explicit_repository = None;
    let mut close_reason = None;
    let mut edit_last = false;
    let mut index = subcommand_index + 1;
    while index < args.len() {
        let value = args[index].to_str().ok_or_else(|| {
            CanonicalizeError::unclassified("non-UTF-8 governed arguments are undeclared")
        })?;
        if tuple == "pr review" {
            if let Some(event) = declared_review_event(value) {
                if review_event.replace(event.to_string()).is_some() {
                    return Err(CanonicalizeError::unclassified(
                        "pr review accepts only one of --approve, --comment, or --request-changes",
                    ));
                }
                index += 1;
                continue;
            }
        }
        if target_and_state && (value == "--delete-branch" || value == "-d") {
            // Branch deletion is a distinct undeclared mutation. Refuse it in
            // the argv scan so it never becomes a field on the routed request.
            return Err(CanonicalizeError::typed(
                RefusalCode::DestructiveFlag,
                format!("{value}: branch deletion stays undeclared"),
            ));
        }
        if fields_only {
            if let Some(flag) = unsupported_create_flag(value) {
                // Refuse before anything is routed: these flags either hand out
                // work (assignment, milestone, project) or need an interactive
                // terminal, and neither is bot speech.
                return Err(CanonicalizeError::typed(
                    RefusalCode::UnsupportedFlag,
                    format!(
                        "{flag}: {tuple} through the shim admits only --title, --body, --body-file, --label, and --repo"
                    ),
                ));
            }
        }
        if tuple == "issue edit" {
            if let Some(flag) = unsupported_issue_edit_flag(value) {
                return Err(CanonicalizeError::typed(
                    RefusalCode::DestructiveFlag,
                    format!("{flag}: repository planning changes stay undeclared"),
                ));
            }
            if let Some((field, supplied)) =
                declared_issue_edit_repeated_value(value, args.get(index + 1))?
            {
                repeated_fields.entry(field).or_default().push(supplied);
                if !value.contains('=') {
                    index += 1;
                }
                index += 1;
                continue;
            }
        }
        if value == "--edit-last" {
            if !is_reviewed_edit_last_tuple(manifest_version, tuple) {
                return Err(CanonicalizeError::unclassified(
                    "undeclared flag --edit-last",
                ));
            }
            if edit_last {
                return Err(CanonicalizeError::unclassified(
                    "--edit-last may be provided only once",
                ));
            }
            edit_last = true;
        } else if value == "--repo" || value == "-R" {
            index += 1;
            let repository = args
                .get(index)
                .and_then(|arg| arg.to_str())
                .ok_or_else(|| CanonicalizeError::unclassified("--repo requires a value"))?;
            explicit_repository = Some(repository.to_string());
        } else if let Some(repository) = attached_repo_value(value) {
            explicit_repository = Some(repository.to_string());
        } else if fields_only {
            if let Some(label) = declared_label_value(value, args.get(index + 1))? {
                labels.push(label);
                if !value.contains('=') {
                    index += 1;
                }
            } else if let Some((field, supplied)) =
                declared_body_value(value, canonical, args.get(index + 1), stdin)?
            {
                body.insert(field, Value::String(supplied));
                if !value.contains('=') {
                    index += 1;
                }
            } else if value.starts_with('-') {
                return Err(CanonicalizeError::unclassified(format!(
                    "undeclared flag {value}"
                )));
            } else {
                positional.push(value.to_string());
            }
        } else if target_and_state {
            if let Some(supplied) = declared_reason_value(value, args.get(index + 1), tuple)? {
                if close_reason.replace(supplied).is_some() {
                    return Err(CanonicalizeError::unclassified(
                        "--reason may be provided only once",
                    ));
                }
                if !value.contains('=') {
                    index += 1;
                }
            } else if let Some((field, supplied)) =
                declared_body_value(value, canonical, args.get(index + 1), stdin)?
            {
                body.insert(field, Value::String(supplied));
                if !value.contains('=') {
                    index += 1;
                }
            } else if value.starts_with('-') {
                return Err(CanonicalizeError::unclassified(format!(
                    "undeclared flag {value}"
                )));
            } else {
                positional.push(value.to_string());
            }
        } else if let Some((field, supplied)) =
            declared_body_value(value, canonical, args.get(index + 1), stdin)?
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
            return Err(CanonicalizeError::unclassified(format!(
                "undeclared flag {value}"
            )));
        } else {
            positional.push(value.to_string());
        }
        index += 1;
    }

    if positional.len() != canonical.target_fields.len() {
        return Err(CanonicalizeError::unclassified(
            "target positional form is undeclared",
        ));
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
        // Thread-state verbs may close or reopen without a comment; the comment
        // field is declared so --comment/--comment-file/-c reuse body plumbing.
        let body_optional_for_state =
            target_and_state && canonical.body_fields.iter().all(|field| field == "comment");
        // A create's required fields and an edit's empty-field rejection belong
        // to upstream. Individual edit fields are optional, while `gh issue
        // create` without --title already fails with its own useful error text.
        let body_optional_for_issue_edit = tuple == "issue edit";
        if !body_optional_for_review
            && !body_optional_for_state
            && !fields_only
            && !body_optional_for_issue_edit
        {
            return Err(CanonicalizeError::unclassified(
                "required declared body field is absent",
            ));
        }
    }
    if tuple == "issue close" && target_and_state {
        let Some(reason) = close_reason else {
            return Err(CanonicalizeError::typed(
                RefusalCode::MissingReason,
                "issue close requires --reason completed|\"not planned\" because those are distinct public statements",
            ));
        };
        body.insert("reason".to_string(), Value::String(reason));
    } else if close_reason.is_some() {
        return Err(CanonicalizeError::unclassified(
            "--reason is only valid for issue close",
        ));
    }
    if let Some(event) = review_event {
        body.insert("event".to_string(), Value::String(event));
    }
    if !labels.is_empty() {
        // The argv flag is singular and repeatable; GitHub's field is a plural
        // array, so the repetitions are collected into one.
        body.insert(
            "labels".to_string(),
            Value::Array(labels.into_iter().map(Value::String).collect()),
        );
    }
    for (field, values) in repeated_fields {
        body.insert(
            field,
            Value::Array(values.into_iter().map(Value::String).collect()),
        );
    }
    // A thread URL positional names its own repository.
    let url_repository = positional
        .iter()
        .find_map(|value| thread_url_repository(value));
    let target = canonical
        .target_fields
        .iter()
        .cloned()
        .zip(positional)
        .map(|(field, value)| (field, Value::String(value)))
        .collect::<Map<_, _>>();
    // A global `--repo` may precede the command head, so inspect the original
    // argv before a command-local flag. The shared resolver then falls back
    // to the URL, GH_REPO, and the working directory's origin.
    let explicit = explicit_repo(args).or(explicit_repository);
    if let (Some(explicit), Some(from_url)) = (&explicit, &url_repository) {
        // The same ambiguity rule as the operator label rows: a URL and a
        // --repo that disagree leave the target a guess.
        if canonical_repository_key(explicit).as_ref() != Some(from_url) {
            return Err(CanonicalizeError::unclassified(format!(
                "the thread URL names {from_url} but --repo names {explicit}"
            )));
        }
    }
    let target_repository = TargetRepository {
        explicit,
        url: url_repository,
        gh_repo: gh_repo_env(),
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let repository = target_repository
        .resolve(&cwd)
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
        author_scope: (tuple == "issue edit").then(|| "own".to_string()),
    })
}

/// Read a `gh pr create` argv into the governed request.
///
/// The request is fields-only: a pull request has no number until it exists.
/// Every admitted flag maps to one body field, and anything else is refused
/// while argv is read, before routing, because a governed route never runs
/// upstream `gh` and silently dropping a flag would change what the caller
/// asked for.
///
/// `--base`, `--head` and `--title` are required here even though upstream
/// `gh` can default or prompt for them: the default base is the repository's
/// default branch and the default head is the local branch, and the shim can
/// look up neither without guessing on the caller's behalf. Whether the head
/// branch exists is GitHub's check, reported back through the route holder.
fn canonicalize_pr_create_from<R: Read>(
    args: &[OsString],
    subcommand_index: usize,
    canonical: &Canonicalization,
    manifest_version: u64,
    stdin: &mut R,
) -> Result<GovernedRequest, CanonicalizeError> {
    if !is_fields_only(canonical)
        || !canonical.target_fields.is_empty()
        || canonical.body_fields != PR_CREATE_BODY_FIELDS
    {
        return Err(CanonicalizeError::unclassified(format!(
            "pr create is declared as {:?} / target {:?} / body {:?}, but this shim reads only fields-only / target [] / body {PR_CREATE_BODY_FIELDS:?}",
            canonical.argv_forms, canonical.target_fields, canonical.body_fields
        )));
    }
    let mut title = None;
    let mut body = None;
    let mut base = None;
    let mut head = None;
    let mut draft = false;
    let mut explicit_repository = None;
    let mut index = subcommand_index + 1;
    while index < args.len() {
        let value = args[index].to_str().ok_or_else(|| {
            CanonicalizeError::unclassified("non-UTF-8 governed arguments are undeclared")
        })?;
        if let Some(flag) = PR_CREATE_UNSUPPORTED_FLAGS
            .iter()
            .copied()
            .find(|flag| value == *flag || value.starts_with(&format!("{flag}=")))
        {
            return Err(CanonicalizeError::typed(
                RefusalCode::UnsupportedFlag,
                format!(
                    "{flag}: pr create through the shim admits only --title, --body, --body-file, --base, --head, --draft, and --repo"
                ),
            ));
        }
        let next = args.get(index + 1);
        let mut consumed_next = false;
        if value == "--draft" || value == "-d" {
            draft = true;
        } else if value == "--repo" || value == "-R" {
            let repository = next
                .and_then(|arg| arg.to_str())
                .ok_or_else(|| CanonicalizeError::unclassified("--repo requires a value"))?;
            explicit_repository = Some(repository.to_string());
            consumed_next = true;
        } else if let Some(repository) = attached_repo_value(value) {
            explicit_repository = Some(repository.to_string());
        } else if let Some((supplied, from_next)) =
            pr_create_flag_value(value, "--title", "-t", next)?
        {
            set_once(&mut title, supplied, "--title")?;
            consumed_next = from_next;
        } else if let Some((supplied, from_next)) =
            pr_create_flag_value(value, "--base", "-B", next)?
        {
            set_once(&mut base, supplied, "--base")?;
            consumed_next = from_next;
        } else if let Some((supplied, from_next)) =
            pr_create_flag_value(value, "--head", "-H", next)?
        {
            set_once(&mut head, supplied, "--head")?;
            consumed_next = from_next;
        } else if let Some((supplied, from_next)) =
            pr_create_flag_value(value, "--body", "-b", next)?
        {
            set_once(&mut body, supplied, "--body/--body-file")?;
            consumed_next = from_next;
        } else if let Some((file, from_next)) =
            pr_create_flag_value(value, "--body-file", "-F", next)?
        {
            // Read the file here and send its text: the route holder cannot
            // see this machine's files, and stdin (`-`) belongs to this process.
            let supplied = read_body_file_from(Path::new(&file), stdin)
                .map_err(|error| CanonicalizeError::unclassified(format!("{value}: {error}")))?;
            set_once(&mut body, supplied, "--body/--body-file")?;
            consumed_next = from_next;
        } else if value.starts_with('-') {
            return Err(CanonicalizeError::unclassified(format!(
                "undeclared flag {value}"
            )));
        } else {
            return Err(CanonicalizeError::unclassified(format!(
                "pr create takes no positional arguments, got {value}"
            )));
        }
        index += if consumed_next { 2 } else { 1 };
    }

    let require = |field: Option<String>, flag: &str, why: &str| {
        field.filter(|value| !value.is_empty()).ok_or_else(|| {
            CanonicalizeError::unclassified(format!(
                "pr create through the shim requires {flag}: {why}"
            ))
        })
    };
    let title = require(
        title,
        "--title",
        "upstream gh would prompt for it, and the governed route has no terminal",
    )?;
    let base = require(
        base,
        "--base",
        "the shim cannot look up the repository's default branch, so it does not guess one",
    )?;
    let head = require(
        head,
        "--head",
        "the shim does not infer the head from the local checkout",
    )?;
    if head.contains(':') {
        // `owner:branch` names a head in another repository (a fork). The bot
        // speaks only for the bound repository, so a cross-repository pull
        // request is refused here rather than left to the route holder.
        return Err(CanonicalizeError::typed(
            RefusalCode::UnsupportedFlag,
            format!(
                "--head {head}: cross-repository heads are refused; the head branch must be in the target repository, named without an owner prefix"
            ),
        ));
    }

    let mut fields = Map::new();
    fields.insert("title".to_string(), Value::String(title));
    if let Some(body) = body {
        fields.insert("body".to_string(), Value::String(body));
    }
    fields.insert("base".to_string(), Value::String(base));
    fields.insert("head".to_string(), Value::String(head));
    fields.insert("draft".to_string(), Value::Bool(draft));

    // Resolve the target repository from a global `--repo` before the command
    // head, then the command-local `--repo`, then GH_REPO, and finally the
    // working directory's origin: the same order `TargetRepository` applies
    // to every other governed verb.
    let target_repository = TargetRepository {
        explicit: explicit_repo(args).or(explicit_repository),
        url: None,
        gh_repo: gh_repo_env(),
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let repository = target_repository
        .resolve(&cwd)
        .map(|repository| {
            canonical_repository_key(&repository)
                .ok_or_else(|| format!("repository {repository} is not owner/name"))
        })
        .transpose()?;
    Ok(GovernedRequest {
        action: "pr create".to_string(),
        target: Map::new(),
        body: fields,
        repository,
        manifest_version,
        edit_last: false,
        author_scope: None,
    })
}

/// The value of one `pr create` string flag in its `--long value`,
/// `--long=value` or `-s value` spelling, and whether it consumed the next
/// argument. The glued short form (`-tTitle`) is not read, so it refuses as an
/// undeclared flag rather than being guessed at.
fn pr_create_flag_value(
    value: &str,
    long: &str,
    short: &str,
    next: Option<&OsString>,
) -> Result<Option<(String, bool)>, CanonicalizeError> {
    if value == long || value == short {
        let supplied = next
            .and_then(|arg| arg.to_str())
            .ok_or_else(|| CanonicalizeError::unclassified(format!("{value} requires a value")))?;
        return Ok(Some((supplied.to_string(), true)));
    }
    Ok(value
        .strip_prefix(long)
        .and_then(|rest| rest.strip_prefix('='))
        .map(|supplied| (supplied.to_string(), false)))
}

/// Upstream `gh` keeps the last of a repeated flag. The shim refuses the
/// repetition instead, so a request never carries a value the caller may not
/// have meant to send.
fn set_once(
    slot: &mut Option<String>,
    supplied: String,
    flag: &str,
) -> Result<(), CanonicalizeError> {
    if slot.replace(supplied).is_some() {
        return Err(CanonicalizeError::unclassified(format!(
            "{flag} may be provided only once"
        )));
    }
    Ok(())
}

/// Canonicalize the one governed API form: an id-addressed PATCH of an issue
/// comment, which is how `github_read`'s comment editor rewrites a comment the
/// calling seat's bot already wrote.
///
/// The endpoint carries the target (repository and comment id) and the only
/// admitted payload is a single `body`, supplied either as a JSON object behind
/// `--input` (including the stdin spelling `-`) or as one `body` field. Every
/// other flag is refused, because the shim does not run upstream `gh` for a
/// governed route and silently dropping a flag would change what the caller
/// asked for.
fn canonicalize_governed_api(
    args: &[OsString],
    tuple: &str,
    canonical: &Canonicalization,
    manifest_version: u64,
) -> Result<GovernedRequest, CanonicalizeError> {
    canonicalize_governed_api_from(
        args,
        tuple,
        canonical,
        manifest_version,
        &mut io::stdin().lock(),
    )
}

fn canonicalize_governed_api_from<R: Read>(
    args: &[OsString],
    tuple: &str,
    canonical: &Canonicalization,
    manifest_version: u64,
    stdin: &mut R,
) -> Result<GovernedRequest, CanonicalizeError> {
    let target_field = canonical
        .target_fields
        .first()
        .ok_or_else(|| CanonicalizeError::unclassified("governed api target is undeclared"))?;
    let body_field = canonical
        .body_fields
        .first()
        .ok_or_else(|| CanonicalizeError::unclassified("governed api body is undeclared"))?;
    let (_, path, _) = api_method_and_path(args)
        .ok_or_else(|| CanonicalizeError::unclassified("api endpoint is undeclared"))?;
    let (repository, comment_id) = own_comment_patch_target(&path).ok_or_else(|| {
        CanonicalizeError::unclassified(
            "only /repos/<owner>/<repo>/issues/comments/<id> is declared for a governed PATCH",
        )
    })?;
    let body_text = own_comment_patch_payload(args, body_field, stdin)?;

    let mut target = Map::new();
    target.insert(target_field.clone(), Value::String(comment_id));
    let mut body = Map::new();
    body.insert(body_field.clone(), Value::String(body_text));
    Ok(GovernedRequest {
        action: tuple.to_string(),
        target,
        body,
        repository: Some(repository),
        manifest_version,
        edit_last: false,
        author_scope: Some("own".to_string()),
    })
}

/// Split `/repos/<owner>/<repo>/issues/comments/<id>` into the canonical
/// repository key and the comment id. Anything else - a full URL, a trailing
/// segment, a non-numeric id - is not the declared endpoint.
fn own_comment_patch_target(path: &str) -> Option<(String, String)> {
    let mut segments = path.strip_prefix('/').unwrap_or(path).split('/');
    (segments.next()? == "repos").then_some(())?;
    let owner = segments.next()?;
    let name = segments.next()?;
    (segments.next()? == "issues").then_some(())?;
    (segments.next()? == "comments").then_some(())?;
    let comment_id = segments.next()?;
    if segments.next().is_some() {
        return None;
    }
    if comment_id.is_empty() || !comment_id.chars().all(|value| value.is_ascii_digit()) {
        return None;
    }
    let repository = canonical_repository_key(&format!("{owner}/{name}"))?;
    Some((repository, comment_id.to_string()))
}

fn own_comment_patch_payload<R: Read>(
    args: &[OsString],
    body_field: &str,
    stdin: &mut R,
) -> Result<String, CanonicalizeError> {
    let mut body = None;
    let mut index = 1;
    while index < args.len() {
        let value = args[index].to_str().ok_or_else(|| {
            CanonicalizeError::unclassified("non-UTF-8 governed arguments are undeclared")
        })?;
        // The method and the endpoint were both matched against the declared
        // rule during classification, so they are skipped rather than reparsed.
        if matches!(value, "--method" | "-X") {
            index += 2;
            continue;
        }
        if value.starts_with("--method=") {
            index += 1;
            continue;
        }
        let (supplied, consumed) = match api_payload_argument(value, args.get(index + 1))? {
            Some(found) => found,
            None => {
                if value.starts_with('-') {
                    return Err(CanonicalizeError::unclassified(format!(
                        "undeclared flag {value}"
                    )));
                }
                index += 1;
                continue;
            }
        };
        let text = match supplied {
            ApiPayload::Field(text) => text,
            ApiPayload::Document(source) => {
                let document = read_body_file_from(Path::new(&source), stdin)
                    .map_err(CanonicalizeError::unclassified)?;
                json_body_only(&document, body_field)?
            }
        };
        if body.replace(text).is_some() {
            return Err(CanonicalizeError::unclassified(format!(
                "the governed comment PATCH admits one {body_field} payload"
            )));
        }
        index += consumed;
    }
    body.ok_or_else(|| {
        CanonicalizeError::unclassified(format!(
            "the governed comment PATCH requires a {body_field}"
        ))
    })
}

enum ApiPayload {
    /// A `body=<text>` field supplied directly on the command line.
    Field(String),
    /// A JSON document behind `--input`, where `-` means standard input.
    Document(String),
}

fn api_payload_argument(
    value: &str,
    next: Option<&OsString>,
) -> Result<Option<(ApiPayload, usize)>, CanonicalizeError> {
    let value_of = |flag: &str| -> Result<(String, usize), CanonicalizeError> {
        next.and_then(|arg| arg.to_str())
            .map(|supplied| (supplied.to_string(), 2))
            .ok_or_else(|| CanonicalizeError::unclassified(format!("{flag} requires a value")))
    };
    if value == "--input" {
        let (supplied, consumed) = value_of(value)?;
        return Ok(Some((ApiPayload::Document(supplied), consumed)));
    }
    if let Some(supplied) = value.strip_prefix("--input=") {
        return Ok(Some((ApiPayload::Document(supplied.to_string()), 1)));
    }
    for flag in ["--field", "--raw-field", "-f", "-F"] {
        let (supplied, consumed) = if value == flag {
            value_of(flag)?
        } else if let Some(rest) = value
            .strip_prefix(&format!("{flag}="))
            .or_else(|| value.strip_prefix(flag).filter(|_| flag.len() == 2))
        {
            (rest.to_string(), 1)
        } else {
            continue;
        };
        let (name, text) = supplied
            .split_once('=')
            .ok_or_else(|| CanonicalizeError::unclassified(format!("{flag} takes name=value")))?;
        if name != "body" {
            return Err(CanonicalizeError::unclassified(format!(
                "{name} is not declared; the governed comment PATCH is body-only"
            )));
        }
        return Ok(Some((ApiPayload::Field(text.to_string()), consumed)));
    }
    Ok(None)
}

/// Read the one declared field out of a JSON payload. A document carrying
/// anything besides that field is not the declared request, and forwarding it
/// would mean routing bytes the shim never interpreted.
fn json_body_only(document: &str, body_field: &str) -> Result<String, CanonicalizeError> {
    let value: Value = serde_json::from_str(document).map_err(|error| {
        CanonicalizeError::unclassified(format!("governed PATCH payload is not JSON: {error}"))
    })?;
    let object = value.as_object().ok_or_else(|| {
        CanonicalizeError::unclassified("governed PATCH payload must be a JSON object")
    })?;
    let text = object
        .get(body_field)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            CanonicalizeError::unclassified(format!(
                "governed PATCH payload needs a string {body_field}"
            ))
        })?;
    if object.len() != 1 {
        return Err(CanonicalizeError::unclassified(format!(
            "governed PATCH payload admits only {body_field}"
        )));
    }
    Ok(text.to_string())
}

fn declared_body_value<R: Read>(
    value: &str,
    canonical: &Canonicalization,
    next: Option<&OsString>,
    stdin: &mut R,
) -> Result<Option<(String, String)>, String> {
    for field in &canonical.body_fields {
        let long = format!("--{field}");
        let short = match field.as_str() {
            "body" => Some("-b"),
            "reaction" => Some("-r"),
            "comment" => Some("-c"),
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
                let supplied = read_body_file_from(Path::new(file), stdin)
                    .map_err(|error| format!("{value}: {error}"))?;
                return Ok(Some((field.clone(), supplied)));
            }
        }

        // Thread-state verbs take an optional comment via --comment-file, including
        // the stdin path `-`, matching the body-file plumbing used for speech.
        if field == "comment" {
            let file = if value == "--comment-file" {
                Some(
                    next.and_then(|arg| arg.to_str())
                        .ok_or_else(|| format!("{value} requires a value"))?,
                )
            } else {
                value.strip_prefix("--comment-file=")
            };
            if let Some(file) = file {
                let supplied = read_body_file_from(Path::new(file), stdin)
                    .map_err(|error| format!("{value}: {error}"))?;
                return Ok(Some((field.clone(), supplied)));
            }
        }
    }
    Ok(None)
}

/// `gh issue create` takes `--label` once per label. The repetitions are
/// collected here rather than through the single-valued body plumbing, which
/// would keep only the last one.
fn declared_label_value(
    value: &str,
    next: Option<&OsString>,
) -> Result<Option<String>, CanonicalizeError> {
    if value == "--labels" || value.starts_with("--labels=") {
        // The declared body field is plural, but the flag upstream accepts is
        // not; refuse rather than let the plural spelling through as one label.
        return Err(CanonicalizeError::unclassified(
            "--labels is not a gh flag; pass --label once per label",
        ));
    }
    if value == "--label" {
        let supplied = next
            .and_then(|arg| arg.to_str())
            .ok_or_else(|| CanonicalizeError::unclassified("--label requires a value"))?;
        return Ok(Some(supplied.to_string()));
    }
    Ok(value.strip_prefix("--label=").map(str::to_string))
}

fn unsupported_create_flag(value: &str) -> Option<&'static str> {
    CREATE_UNSUPPORTED_FLAGS
        .iter()
        .copied()
        .find(|flag| value == *flag || value.starts_with(&format!("{flag}=")))
}

fn unsupported_issue_edit_flag(value: &str) -> Option<&'static str> {
    ISSUE_EDIT_DESTRUCTIVE_FLAGS
        .iter()
        .copied()
        .find(|flag| value == *flag || value.starts_with(&format!("{flag}=")))
}

fn declared_issue_edit_repeated_value(
    value: &str,
    next: Option<&OsString>,
) -> Result<Option<(String, String)>, CanonicalizeError> {
    for (flag, field) in [
        ("--add-label", "add_labels"),
        ("--remove-label", "remove_labels"),
        ("--add-assignee", "add_assignees"),
        ("--remove-assignee", "remove_assignees"),
    ] {
        if value == flag {
            let supplied = next.and_then(|arg| arg.to_str()).ok_or_else(|| {
                CanonicalizeError::unclassified(format!("{flag} requires a value"))
            })?;
            return Ok(Some((field.to_string(), supplied.to_string())));
        }
        if let Some(supplied) = value.strip_prefix(&format!("{flag}=")) {
            return Ok(Some((field.to_string(), supplied.to_string())));
        }
    }
    Ok(None)
}

fn declared_reason_value(
    value: &str,
    next: Option<&OsString>,
    tuple: &str,
) -> Result<Option<String>, CanonicalizeError> {
    let supplied = if value == "--reason" {
        Some(
            next.and_then(|arg| arg.to_str())
                .ok_or_else(|| CanonicalizeError::unclassified("--reason requires a value"))?
                .to_string(),
        )
    } else {
        value.strip_prefix("--reason=").map(str::to_string)
    };
    let Some(supplied) = supplied else {
        return Ok(None);
    };
    if tuple != "issue close" {
        return Err(CanonicalizeError::unclassified(
            "--reason is only valid for issue close",
        ));
    }
    // Upstream gh documents the value as "not planned", with a space, while the
    // GitHub API's state_reason and the holder contract use "not_planned". Accept
    // both spellings and carry the API form, so a command copied from gh's own
    // help is not refused.
    let supplied = if supplied == "not planned" {
        "not_planned".to_string()
    } else {
        supplied
    };
    if !ISSUE_CLOSE_REASONS.contains(&supplied.as_str()) {
        return Err(CanonicalizeError::unclassified(
            "--reason must be completed or \"not planned\"",
        ));
    }
    Ok(Some(supplied))
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
        if let Some(repository) = attached_repo_value(value) {
            return Some(repository.to_string());
        }
    }
    None
}

/// The value of `--repo=<r>`, or of the shorthand spelling upstream `gh`
/// also accepts, `-R=<r>`. The glued `-R<r>` form is not read: a comment body
/// such as `-Really?` would otherwise be taken for a repository.
fn attached_repo_value(value: &str) -> Option<&str> {
    value
        .strip_prefix("--repo=")
        .or_else(|| value.strip_prefix("-R="))
        .filter(|repository| !repository.is_empty())
}

/// The repository when the command names none itself: `GH_REPO`, else the
/// working directory's origin. Same order and resolver as a full invocation.
fn infer_repository_from_git() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    TargetRepository {
        gh_repo: gh_repo_env(),
        ..TargetRepository::default()
    }
    .resolve(&cwd)
}

fn gh_repo_env() -> Option<String> {
    std::env::var("GH_REPO")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Where a `gh` invocation says which repository it acts on, collected before
/// any git lookup.
///
/// Resolution follows upstream `gh`: `-R`/`--repo`, then a thread URL (or a
/// `gh api` endpoint) naming a repository, then `GH_REPO`, and only then the
/// working directory's `origin` remote. The agent binding and the repository
/// sent on a governed route both come from this one resolver, so a command
/// aimed at a bound repository is governed as that repository wherever it
/// runs from, and can never be spoken as the working directory's bot.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TargetRepository {
    /// `-R`/`--repo` exactly as spelled.
    explicit: Option<String>,
    /// Canonical `owner/name` from a thread URL or an API endpoint.
    url: Option<String>,
    /// `GH_REPO` exactly as set.
    gh_repo: Option<String>,
}

impl TargetRepository {
    fn from_invocation(args: &[OsString]) -> Self {
        Self {
            explicit: explicit_repo(args),
            url: positional_target_repository(args),
            gh_repo: gh_repo_env(),
        }
    }

    /// The repository the command names itself, without consulting git.
    fn named(&self) -> Option<&str> {
        self.explicit
            .as_deref()
            .or(self.url.as_deref())
            .or(self.gh_repo.as_deref())
    }

    /// The named repository as spelled, else the canonical key of the
    /// working directory's origin. A name that is not a github.com
    /// `owner/name` (another host, a malformed value) is returned as spelled
    /// and never falls back to the origin: the command is not aimed at the
    /// checkout it happens to run in.
    fn resolve(&self, cwd: &Path) -> Option<String> {
        match self.named() {
            Some(named) => Some(named.to_string()),
            None => repository_key_from_origin(&project_root_for(cwd)),
        }
    }

    /// Canonical `owner/name` of the target, or `None` when it is unresolvable
    /// or not a github.com repository.
    fn repository_key(&self, cwd: &Path) -> Option<String> {
        canonical_repository_key(&self.resolve(cwd)?)
    }
}

/// Canonical `owner/name` of a `https://github.com/<owner>/<repo>/issues/<n>`
/// or `.../pull/<n>` URL. Trailing path segments, a query, or a fragment
/// (`#issuecomment-1`) do not change which repository the URL names.
fn thread_url_repository(value: &str) -> Option<String> {
    let path = ["https://github.com/", "http://github.com/"]
        .iter()
        .find_map(|prefix| value.strip_prefix(prefix))?;
    let mut segments = path.split('/');
    let owner = segments.next()?;
    let name = segments.next()?;
    if !matches!(segments.next()?, "issues" | "pull") {
        return None;
    }
    let number = segments.next()?;
    let digits = number.split(['#', '?']).next().unwrap_or("");
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    canonical_repository_key(&format!("{owner}/{name}"))
}

/// Canonical `owner/name` of a `/repos/<owner>/<repo>/...` API endpoint. The
/// `{owner}`/`{repo}` placeholders are filled by upstream `gh` from the other
/// sources, so an endpoint using them names nothing itself.
fn api_endpoint_repository(path: &str) -> Option<String> {
    let mut segments = path.strip_prefix('/').unwrap_or(path).split('/');
    (segments.next()? == "repos").then_some(())?;
    let owner = segments.next()?;
    let name = segments.next()?;
    if owner.contains(['{', '}']) || name.contains(['{', '}']) {
        return None;
    }
    canonical_repository_key(&format!("{owner}/{name}"))
}

/// Flags of the issue and pull request verbs whose next argument is their
/// value rather than a positional. A thread URL passed as `--body <url>` is
/// prose, not the target. A flag outside this list is read as a switch, so an
/// unknown flag's URL value can at worst make the command look aimed at that
/// URL's repository; the governed route then refuses the mismatch by name
/// instead of running upstream `gh` as the operator.
const VALUE_FLAGS: &[&str] = &[
    "--body",
    "-b",
    "--body-file",
    "-F",
    "--title",
    "-t",
    "--label",
    "-l",
    "--add-label",
    "--remove-label",
    "--assignee",
    "-a",
    "--add-assignee",
    "--remove-assignee",
    "--milestone",
    "-m",
    "--project",
    "-p",
    "--add-project",
    "--remove-project",
    "--comment",
    "-c",
    "--reason",
    "-r",
    "--reaction",
    "--template",
    "-T",
    "--base",
    "-B",
    "--head",
    "-H",
    "--json",
    "--jq",
    "-q",
    "--repo",
    "-R",
    "--hostname",
    "--config-dir",
];

/// Repository named by a positional thread URL or, for `gh api`, by the
/// endpoint.
fn positional_target_repository(args: &[OsString]) -> Option<String> {
    let (verb, subcommand, head_index) = command_head(args)?;
    if verb == "api" {
        let (_, path, _) = api_method_and_path(&args[head_index..])?;
        return api_endpoint_repository(&path);
    }
    if verb == "repo" {
        return subcommand
            .as_deref()
            .filter(|subcommand| REPO_POSITIONAL_TARGET_SUBCOMMANDS.contains(subcommand))
            .and_then(|_| repo_positional_repository(&args[head_index + 1..]));
    }
    // `pr review --comment` is a switch selecting the review event, not a
    // comment value as it is on the close and reopen verbs.
    let comment_is_switch = verb == "pr" && subcommand.as_deref() == Some("review");
    let mut skip_value = false;
    for arg in args.iter().skip(head_index + 1) {
        let value = arg.to_str()?;
        if std::mem::take(&mut skip_value) {
            continue;
        }
        if value.starts_with('-') {
            let is_switch = comment_is_switch && matches!(value, "--comment" | "-c");
            skip_value = !value.contains('=') && !is_switch && VALUE_FLAGS.contains(&value);
            continue;
        }
        if let Some(repository) = thread_url_repository(value) {
            return Some(repository);
        }
    }
    None
}

/// Canonical `owner/name` of the repository positional of a `gh repo`
/// subcommand (arguments after `repo`): the first positional after the
/// subcommand, as `owner/name` or a github.com URL. A bare name (`gh repo
/// create foo`) names no owner and resolves to nothing.
fn repo_positional_repository(args_after_verb: &[OsString]) -> Option<String> {
    let mut skip_value = false;
    let mut seen_subcommand = false;
    for arg in args_after_verb {
        let value = arg.to_str()?;
        if std::mem::take(&mut skip_value) {
            continue;
        }
        // Everything after `--` is handed to git, not read by `gh`.
        if value == "--" {
            return None;
        }
        if value.starts_with('-') {
            skip_value = !value.contains('=') && REPO_VALUE_FLAGS.contains(&value);
            continue;
        }
        if !std::mem::replace(&mut seen_subcommand, true) {
            continue;
        }
        return canonical_repository_key(value);
    }
    None
}

/// Which kind of thread a label-only edit targets: `gh issue edit` or
/// `gh pr edit`. Both verbs share one parser; this carries the differences in
/// what the positional may name and how the audit line records it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LabelTarget {
    Issue,
    PullRequest,
}

impl LabelTarget {
    fn row_text(self) -> &'static str {
        match self {
            Self::Issue => OPERATOR_LABEL_ROW_TEXT,
            Self::PullRequest => OPERATOR_PR_LABEL_ROW_TEXT,
        }
    }

    /// What the positional names, for refusal text.
    fn noun(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::PullRequest => "pull request",
        }
    }

    /// The path segment of a `https://github.com/<owner>/<repo>/<segment>/<n>`
    /// URL that names this kind of thread.
    fn url_segment(self) -> &'static str {
        match self {
            Self::Issue => "issues",
            Self::PullRequest => "pull",
        }
    }

    /// The audit field that carries the number. The issue row keeps the
    /// `issue_number` field it has always written, so existing readers of the
    /// audit log see the same record shape.
    fn audit_number_field(self) -> &'static str {
        match self {
            Self::Issue => "issue_number",
            Self::PullRequest => "pr_number",
        }
    }
}

/// A label-only `gh issue edit` or `gh pr edit` accepted for the operator
/// bypass: what the audit line records before upstream `gh` runs.
#[derive(Debug, Eq, PartialEq)]
struct OperatorLabelEdit {
    /// From an issue or pull request URL, else from `--repo`/`-R`; `None`
    /// means the caller falls back to the git origin, as upstream `gh` does.
    repository: Option<String>,
    target: LabelTarget,
    number: u64,
    labels_added: Vec<String>,
    labels_removed: Vec<String>,
}

const OPERATOR_LABEL_ROW_TEXT: &str = "under GH_SHIM_BYPASS=operator `gh issue edit` admits only label changes: --add-label, --remove-label, one issue number or URL, and --repo/-R";
const OPERATOR_PR_LABEL_ROW_TEXT: &str = "under GH_SHIM_BYPASS=operator `gh pr edit` admits only label changes: --add-label, --remove-label, one pull request number or URL, and --repo/-R";
const OPERATOR_LABEL_CREATE_ROW_TEXT: &str = "under GH_SHIM_BYPASS=operator `gh label create` admits only one label name, --color/-c, --description/-d, --force/-f, and --repo/-R";

/// Read the argv of the operator label row, refusing everything that is not
/// part of it. `gh issue edit` and `gh pr edit` share this reader;
/// `target_kind` says whether the positional must name an issue or a pull
/// request.
///
/// Accepted, and nothing else: `--add-label` and `--remove-label` as
/// `--flag value` or `--flag=value` with comma-separated labels; exactly one
/// positional, a number or a URL naming an issue (`/issues/<n>`) or a pull
/// request (`/pull/<n>`) to match `target`; `--repo`/`-R` as `--repo value`,
/// `--repo=value`, `-R value` or `-R=value`, before or after the command. At
/// least one label flag is required. Any other argument refuses by name even
/// when a label flag is also present, because upstream `gh` runs the whole
/// argv: a title, body, reviewer or assignee change riding along with a label
/// would run under the operator's identity without being recorded as such. A
/// pull request branch name is not admitted either: the audit line must record
/// the number that was changed.
fn parse_operator_label_edit(
    args: &[OsString],
    target_kind: LabelTarget,
) -> Result<OperatorLabelEdit, CanonicalizeError> {
    let row_text = target_kind.row_text();
    let noun = target_kind.noun();
    let (_, _, head_index) = command_head(args)
        .ok_or_else(|| CanonicalizeError::unclassified("missing command head"))?;
    let mut explicit_repository: Option<String> = None;
    let mut target: Option<&str> = None;
    let mut labels_added = Vec::new();
    let mut labels_removed = Vec::new();
    let mut saw_label_flag = false;
    let mut index = 0;
    while index < args.len() {
        // The two command words (`issue edit` or `pr edit`); command_head
        // found them.
        if index == head_index || index == head_index + 1 {
            index += 1;
            continue;
        }
        let value = args[index].to_str().ok_or_else(|| {
            CanonicalizeError::unclassified("non-UTF-8 arguments are outside the label row")
        })?;
        let next = args.get(index + 1);
        if let Some((supplied, consumed)) = operator_row_flag_value(value, "--add-label", next)?
            .or(operator_row_flag_value(value, "--remove-label", next)?)
        {
            let flag = value.split('=').next().unwrap_or(value);
            let labels = supplied
                .split(',')
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            if labels.is_empty() {
                return Err(CanonicalizeError::typed(
                    RefusalCode::UnsupportedFlag,
                    format!("{flag}: names no label"),
                ));
            }
            if flag == "--add-label" {
                labels_added.extend(labels);
            } else {
                labels_removed.extend(labels);
            }
            saw_label_flag = true;
            index += consumed;
            continue;
        }
        if let Some(consumed) = operator_row_repo_flag(value, next, &mut explicit_repository)? {
            index += consumed;
            continue;
        }
        if value.starts_with('-') {
            return Err(CanonicalizeError::typed(
                RefusalCode::UnsupportedFlag,
                format!("{}: {row_text}", refused_flag_name(value)),
            ));
        }
        if let Some(first) = target {
            return Err(CanonicalizeError::unclassified(format!(
                "{value}: a second positional after {noun} {first}; {row_text}"
            )));
        }
        target = Some(value);
        index += 1;
    }

    if !saw_label_flag {
        return Err(CanonicalizeError::typed(
            RefusalCode::UnsupportedFlag,
            format!("no --add-label or --remove-label: {row_text}"),
        ));
    }
    let target = target.ok_or_else(|| {
        CanonicalizeError::unclassified(format!("no {noun} number or URL: {row_text}"))
    })?;
    let segment = target_kind.url_segment();
    let (url_repository, number) = parse_thread_target(target, segment).ok_or_else(|| {
        CanonicalizeError::unclassified(format!(
            "{target}: not a number or https://github.com/<owner>/<repo>/{segment}/<number> URL"
        ))
    })?;
    let explicit_repository = explicit_repository
        .map(|repository| canonical_repository_key(&repository).unwrap_or(repository));
    // A URL names its own repository. If --repo names a different one the
    // target is ambiguous, and the audit line would record a guess.
    if let (Some(from_url), Some(explicit)) = (&url_repository, &explicit_repository) {
        if from_url != explicit {
            return Err(CanonicalizeError::unclassified(format!(
                "{target}: the {noun} URL names {from_url} but --repo names {explicit}"
            )));
        }
    }
    Ok(OperatorLabelEdit {
        repository: url_repository.or(explicit_repository),
        target: target_kind,
        number,
        labels_added,
        labels_removed,
    })
}

/// A `gh label create` accepted for the operator bypass: what the audit line
/// records before upstream `gh` runs.
#[derive(Debug, Eq, PartialEq)]
struct OperatorLabelCreate {
    /// From `--repo`/`-R`; `None` means the caller falls back to the git
    /// origin, as upstream `gh` does.
    repository: Option<String>,
    label: String,
    /// `None` when no color was given: upstream then picks a random one.
    color: Option<String>,
}

/// Read the argv of the operator label-create row, refusing everything that
/// is not part of it.
///
/// Accepted, and nothing else (the flags `gh label create --help` lists):
/// exactly one positional, the label name; `--color`/`-c` and
/// `--description`/`-d` with a value, as `flag value` or `flag=value`;
/// `--force`/`-f` without a value; `--repo`/`-R` as for the label edit row.
/// Every other flag and a second positional refuse by name, because upstream
/// `gh` runs the whole argv under the operator's identity.
fn parse_operator_label_create(
    args: &[OsString],
) -> Result<OperatorLabelCreate, CanonicalizeError> {
    let row_text = OPERATOR_LABEL_CREATE_ROW_TEXT;
    let (_, _, head_index) = command_head(args)
        .ok_or_else(|| CanonicalizeError::unclassified("missing command head"))?;
    let mut explicit_repository: Option<String> = None;
    let mut label: Option<&str> = None;
    let mut color: Option<String> = None;
    let mut saw_description = false;
    let mut saw_force = false;
    let mut index = 0;
    while index < args.len() {
        // The two command words (`label`, `create`); command_head found them.
        if index == head_index || index == head_index + 1 {
            index += 1;
            continue;
        }
        let value = args[index].to_str().ok_or_else(|| {
            CanonicalizeError::unclassified("non-UTF-8 arguments are outside the label row")
        })?;
        let next = args.get(index + 1);
        if let Some((supplied, consumed)) = operator_row_flag_value(value, "--color", next)?
            .or(operator_row_flag_value(value, "-c", next)?)
        {
            if color.replace(supplied).is_some() {
                return Err(CanonicalizeError::typed(
                    RefusalCode::UnsupportedFlag,
                    "--color: given more than once",
                ));
            }
            index += consumed;
            continue;
        }
        if let Some((_, consumed)) = operator_row_flag_value(value, "--description", next)?
            .or(operator_row_flag_value(value, "-d", next)?)
        {
            if std::mem::replace(&mut saw_description, true) {
                return Err(CanonicalizeError::typed(
                    RefusalCode::UnsupportedFlag,
                    "--description: given more than once",
                ));
            }
            index += consumed;
            continue;
        }
        if value == "--force" || value == "-f" {
            if std::mem::replace(&mut saw_force, true) {
                return Err(CanonicalizeError::typed(
                    RefusalCode::UnsupportedFlag,
                    "--force: given more than once",
                ));
            }
            index += 1;
            continue;
        }
        if let Some(consumed) = operator_row_repo_flag(value, next, &mut explicit_repository)? {
            index += consumed;
            continue;
        }
        if value.starts_with('-') {
            return Err(CanonicalizeError::typed(
                RefusalCode::UnsupportedFlag,
                format!("{}: {row_text}", refused_flag_name(value)),
            ));
        }
        if let Some(first) = label {
            return Err(CanonicalizeError::unclassified(format!(
                "{value}: a second positional after label {first}; {row_text}"
            )));
        }
        label = Some(value);
        index += 1;
    }
    let label = label
        .ok_or_else(|| CanonicalizeError::unclassified(format!("no label name: {row_text}")))?;
    Ok(OperatorLabelCreate {
        repository: explicit_repository
            .map(|repository| canonical_repository_key(&repository).unwrap_or(repository)),
        label: label.to_string(),
        color,
    })
}

/// Consume `--repo`/`-R` for an operator row, refusing a second one. Returns
/// how many arguments the spelling used, or `None` when `value` is a
/// different argument.
fn operator_row_repo_flag(
    value: &str,
    next: Option<&OsString>,
    explicit_repository: &mut Option<String>,
) -> Result<Option<usize>, CanonicalizeError> {
    let Some((supplied, consumed)) = operator_row_flag_value(value, "--repo", next)?
        .or(operator_row_flag_value(value, "-R", next)?)
    else {
        return Ok(None);
    };
    if explicit_repository.replace(supplied).is_some() {
        return Err(CanonicalizeError::typed(
            RefusalCode::UnsupportedFlag,
            "--repo: given more than once",
        ));
    }
    Ok(Some(consumed))
}

/// The name of a refused flag without any value it carried: `--body=...` and
/// the attached short spelling `-bTEXT` must not echo the text.
fn refused_flag_name(value: &str) -> &str {
    if value.starts_with("--") {
        value.split('=').next().unwrap_or(value)
    } else {
        value.get(..2).unwrap_or(value)
    }
}

/// The value of `flag` when `value` is that flag, spelled `flag value` (the
/// value is the next argument) or `flag=value`, and how many arguments the
/// spelling used. `None` when `value` is a different argument. A missing,
/// empty or flag-shaped value refuses rather than letting upstream `gh`
/// interpret it differently.
fn operator_row_flag_value(
    value: &str,
    flag: &str,
    next: Option<&OsString>,
) -> Result<Option<(String, usize)>, CanonicalizeError> {
    let (supplied, consumed) = if value == flag {
        (next.and_then(|arg| arg.to_str()), 2)
    } else if let Some(inline) = value
        .strip_prefix(flag)
        .and_then(|rest| rest.strip_prefix('='))
    {
        (Some(inline), 1)
    } else {
        return Ok(None);
    };
    match supplied {
        Some(supplied) if !supplied.is_empty() && !supplied.starts_with('-') => {
            Ok(Some((supplied.to_string(), consumed)))
        }
        _ => Err(CanonicalizeError::typed(
            RefusalCode::UnsupportedFlag,
            format!("{flag}: requires a value"),
        )),
    }
}

/// A thread number, or a `https://github.com/<owner>/<repo>/<segment>/<number>`
/// URL together with the repository it names. `segment` is `issues` for an
/// issue and `pull` for a pull request, so a URL of the other kind refuses.
fn parse_thread_target(target: &str, segment: &str) -> Option<(Option<String>, u64)> {
    let number = |text: &str| {
        (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| text.parse::<u64>().ok())
            .flatten()
            .filter(|number| *number > 0)
    };
    if let Some(issue_number) = number(target) {
        return Some((None, issue_number));
    }
    let path = ["https://github.com/", "http://github.com/"]
        .iter()
        .find_map(|prefix| target.strip_prefix(prefix))?
        .trim_end_matches('/');
    let parts = path.split('/').collect::<Vec<_>>();
    let [owner, repository, kind, issue] = parts.as_slice() else {
        return None;
    };
    if *kind != segment {
        return None;
    }
    let repository = canonical_repository_key(&format!("{owner}/{repository}"))?;
    Some((Some(repository), number(issue)?))
}

#[derive(Debug)]
enum RouteOutcome {
    Result(String),
    /// Native close/reopen confirmations go to stderr, as upstream gh does.
    ResultStderr(String),
    StateAppliedCommentFailed(String),
    UpstreamError(String),
    Refusal(String),
    UnboundIdentity,
    SchemaMismatch(String),
    GovernanceUnavailable,
    GovernanceUnavailableTimedOut {
        stage: ProbeStage,
        elapsed_ms: u64,
    },
    OutcomeUnknown {
        elapsed_ms: u64,
    },
    Unavailable(String),
    /// The command carries no agent session ticket, so it cannot speak.
    NoAgentSession,
    /// The relay or plexus refused. `code` is recorded for `gh --status`;
    /// `text` names the code and what it means for the caller.
    RelayRefusal {
        code: String,
        text: String,
    },
    /// The relay reached plexus but plexus could not tell whether the write
    /// happened. Never resent.
    OutcomeUndetermined(String),
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

/// Carry a governed request to plexus through the AFT daemon's relay. The
/// details live in `relay_client`; this wrapper keeps the rung-record update
/// that marks the daemon as recently reachable after a result.
#[allow(clippy::too_many_arguments)]
fn route_governed(
    paths: &StatePaths,
    determination: &RungRecord,
    agent_binding: &AgentBinding,
    request: GovernedRequest,
    now: u64,
    manifest: &Manifest,
    relay: &RelayContext,
) -> RouteOutcome {
    let outcome = relay_client::route(
        paths,
        determination,
        agent_binding,
        request,
        now,
        manifest,
        relay,
    );
    if matches!(
        &outcome,
        RouteOutcome::Result(_)
            | RouteOutcome::ResultStderr(_)
            | RouteOutcome::StateAppliedCommentFailed(_)
    ) && determination.rung == Rung::R3
    {
        let mut updated = determination.clone();
        updated.as_of_unix_secs = now;
        updated.last_reachable_unix_secs = Some(now);
        write_rung_record_silently(paths, &updated);
    }
    outcome
}

#[path = "gh_shim_relay_client.rs"]
mod relay_client;
use relay_client::RelayContext;

fn refuse_governance_unavailable(
    paths: &StatePaths,
    agent_binding: &AgentBinding,
    now: u64,
    text: &str,
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
    refuse(RefusalCode::GovernanceUnavailable, text)
}

fn outcome_unknown_text(elapsed_ms: u64) -> String {
    format!(
        "the governed request was sent but no reply arrived within {elapsed_ms} ms — it may have executed; check before retrying (for comments: gh api repos/<owner>/<repo>/issues/<n>/comments --jq '.[-1]')"
    )
}

fn refuse_outcome_unknown(
    paths: &StatePaths,
    agent_binding: &AgentBinding,
    now: u64,
    elapsed_ms: u64,
) -> i32 {
    refuse_outcome_unknown_with_text(paths, agent_binding, now, &outcome_unknown_text(elapsed_ms))
}

fn refuse_outcome_unknown_with_text(
    paths: &StatePaths,
    agent_binding: &AgentBinding,
    now: u64,
    text: &str,
) -> i32 {
    let state = SeamState {
        bound_holder: seam_state(paths).bound_holder,
        agent_binding: Some(agent_binding.clone()),
        last_seam_refusal: Some(LastSeamRefusal {
            code: RefusalCode::OutcomeUnknown.as_str().to_string(),
            at_unix_secs: now,
        }),
    };
    if let Err(error) = write_seam_state(paths, state) {
        return refuse(
            RefusalCode::SeamUnavailable,
            &format!("governed self-report update failed: {error}"),
        );
    }
    refuse(RefusalCode::OutcomeUnknown, text)
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
    crate::private_storage::open_root(&paths.root)?;
    let bytes = serde_json::to_vec(&state).map_err(io::Error::other)?;
    let temporary = paths.seam_state.with_extension("tmp");
    let mut file = crate::private_storage::options()
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
    let metadata = json!({
        "agent_id": agent_id,
        "pid": std::process::id(),
    });
    if V12_GOVERNED_TUPLES.contains(&request.action.as_str()) {
        // Thread-state verbs use verb/repository/number/reason/comment instead of
        // action/target/body so a delete-branch flag cannot appear on the wire.
        let mut wire = json!({
            "operation": ROUTING_OPERATION,
            "gh_route_schema": 1,
            "verb": request.action,
            "repository": request.repository,
            "number": request.target.get("number").cloned().unwrap_or(Value::Null),
            "manifest_version": request.manifest_version,
            "rung_as_of_unix_secs": determination.as_of_unix_secs,
            "metadata": metadata,
        });
        if request.action == "issue close" {
            if let Some(reason) = request.body.get("reason").cloned() {
                wire["reason"] = reason;
            }
        }
        if let Some(comment) = request.body.get("comment").cloned() {
            wire["comment"] = comment;
        }
        return wire;
    }
    let edit_last = request.edit_last;
    let author_scope = request.author_scope;
    let mut wire = json!({
        "operation": ROUTING_OPERATION,
        "gh_route_schema": 1,
        "action": request.action,
        "target": request.target,
        "body": request.body,
        "repository": request.repository,
        "manifest_version": request.manifest_version,
        "rung_as_of_unix_secs": determination.as_of_unix_secs,
        "metadata": metadata,
    });
    if let Some(author_scope) = author_scope {
        wire["author_scope"] = Value::String(author_scope);
    }
    // Keep the create wire shape byte-for-byte compatible. The explicit marker
    // lets the route holder perform the same authenticated-user-only mutation
    // that gh's native --edit-last flag requests.
    if edit_last {
        wire["edit_last"] = Value::Bool(true);
    }
    wire
}

/// Parser for the retired prefrontal `gh.route` reply. Governed writes now go
/// through the AFT daemon's relay (see `relay_client`); this parser and the
/// renderers only it reaches, including the partial "state applied, comment
/// failed" one that the relay path can never produce, are kept with their
/// tests rather than deleted.
#[cfg_attr(not(test), allow(dead_code))]
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
        Some("applied") => Ok(RouteOutcome::Result(render_applied_state(object)?)),
        Some("state_applied_comment_failed") => Ok(RouteOutcome::StateAppliedCommentFailed(
            render_state_applied_comment_failed(object)?,
        )),
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

fn returned_state_fields(
    object: &Map<String, Value>,
) -> Result<(String, Option<String>), RouteOutcome> {
    let state = object
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RouteOutcome::SchemaMismatch("governance seam omitted returned state".to_string())
        })?
        .to_string();
    let state_reason = object
        .get("state_reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok((state, state_reason))
}

fn render_applied_state(object: &Map<String, Value>) -> Result<String, RouteOutcome> {
    let (state, state_reason) = returned_state_fields(object)?;
    let mut output = state;
    output.push('\n');
    if let Some(reason) = state_reason {
        output.push_str(&reason);
        output.push('\n');
    }
    Ok(output)
}

fn render_state_applied_comment_failed(
    object: &Map<String, Value>,
) -> Result<String, RouteOutcome> {
    let (state, state_reason) = returned_state_fields(object)?;
    let comment_error = object
        .get("comment_error")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            RouteOutcome::SchemaMismatch(
                "governance seam omitted comment_error on partial state apply".to_string(),
            )
        })?;
    let code = comment_error_code(comment_error.get("code"))
        .ok_or_else(|| RouteOutcome::SchemaMismatch("comment_error omitted code".to_string()))?;
    let detail = comment_error
        .get("detail")
        .and_then(Value::as_str)
        .ok_or_else(|| RouteOutcome::SchemaMismatch("comment_error omitted detail".to_string()))?;
    let mut output = format!("APPLIED {state}\n");
    if let Some(reason) = state_reason {
        output.push_str(&reason);
        output.push('\n');
    }
    output.push_str("comment_error: ");
    output.push_str(&code);
    output.push_str(": ");
    output.push_str(detail);
    output.push('\n');
    Ok(output)
}

fn comment_error_code(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(code) => Some(code.clone()),
        Value::Number(code) => Some(code.to_string()),
        _ => None,
    }
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
    append_bypass_audit_record(
        paths,
        &json!({
            "as_of_unix_secs": now,
            "tuple": tuple,
            "repository": repository,
        }),
    )
}

/// The operator label row's audit line: the common bypass fields plus the
/// issue or pull request number and the labels added and removed, so
/// `gh --status` shows what the operator changed and not just that a bypass
/// happened.
fn append_label_bypass_audit(
    paths: &StatePaths,
    tuple: &str,
    repository: Option<&str>,
    edit: &OperatorLabelEdit,
    now: u64,
) -> io::Result<()> {
    let mut record = json!({
        "as_of_unix_secs": now,
        "tuple": tuple,
        "repository": repository,
        "labels_added": edit.labels_added,
        "labels_removed": edit.labels_removed,
    });
    record[edit.target.audit_number_field()] = json!(edit.number);
    append_bypass_audit_record(paths, &record)
}

fn append_bypass_audit_record(paths: &StatePaths, record: &Value) -> io::Result<()> {
    crate::private_storage::open_root(&paths.root)?;
    let mut record = serde_json::to_vec(record).map_err(io::Error::other)?;
    record.push(b'\n');
    let mut file = crate::private_storage::options()
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
    last_probe: Option<LastProbeReport>,
    bypass_audit: Option<Vec<Value>>,
    bypass_audit_error: Option<String>,
    executing_image: Option<String>,
    executing_image_error: Option<String>,
    real_gh_resolution: Option<RealGhResolution>,
    real_gh_resolution_error: Option<String>,
    manifests_retained: usize,
    manifests_dir: String,
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
    let last_probe = if disabled {
        None
    } else {
        read_last_probe(paths)
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
        last_probe,
        bypass_audit,
        bypass_audit_error,
        executing_image: image
            .as_ref()
            .ok()
            .map(|path| path.to_string_lossy().into_owned()),
        executing_image_error: image.err(),
        real_gh_resolution,
        real_gh_resolution_error,
        manifests_retained: retained_manifest_count(paths),
        manifests_dir: paths.manifests_dir.to_string_lossy().into_owned(),
    }
}

/// Number of retained signed manifests on disk. A directory read failure or a
/// non-regular entry is not a security boundary, so this degrades to zero
/// rather than failing the whole self-report.
fn retained_manifest_count(paths: &StatePaths) -> usize {
    fs::read_dir(&paths.manifests_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
                .count()
        })
        .unwrap_or(0)
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
    let _ = crate::private_storage::open_root(&paths.root);
    let temporary = paths.unexpected_gh_route_advertisers.with_extension("tmp");
    if crate::private_storage::write(&temporary, bytes).is_ok() {
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
    let shims_dir = crate::environment::non_empty_os_var("AFT_GH_SHIMS_DIR").map(PathBuf::from);
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
    match code {
        RefusalCode::OutcomeUnknown => OUTCOME_UNKNOWN_EXIT_STATUS,
        _ => REFUSAL_EXIT_STATUS,
    }
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
// Release-profile test builds carry no dev key in the trust set, so the tests
// that verify under it are `cfg(debug_assertions)` and the helpers only they
// use read as dead there; the module compiles in both profiles.
#[cfg_attr(not(debug_assertions), allow(dead_code))]
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
    const BRANCH_PROTECTION_PATH_GLOB: &str = "/repos/*/*/branches/*/protection";
    const BRANCH_PROTECTION_API_TUPLE: &str = "api:PUT:/repos/*/*/branches/*/protection";

    struct ScopedTestEnvVar {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl ScopedTestEnvVar {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let previous = std::env::var_os(key);
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
            Self { key, previous }
        }
    }

    impl Drop for ScopedTestEnvVar {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(previous) => unsafe { std::env::set_var(self.key, previous) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn fixture_manifest() -> Manifest {
        serde_json::from_str(include_str!(
            "../tests/fixtures/gh_shim/initial-manifest-v1.json"
        ))
        .expect("initial manifest fixture")
    }

    #[test]
    fn repeated_manifest_resolution_verifies_and_writes_once() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(root.path().to_path_buf());
        write_signed_manifest(&paths, fixture_manifest(), TEST_NOW);
        VERIFIED_ENVELOPE.with(|cached| *cached.borrow_mut() = None);
        MANIFEST_WORK.with(|count| count.set((0, 0)));
        for _ in 0..3 {
            load_manifest(&paths, TEST_NOW).unwrap();
        }
        assert_eq!(
            MANIFEST_WORK.with(std::cell::Cell::get),
            (1, 1),
            "signature verifications, last-valid rewrites"
        );
    }

    #[test]
    fn manifest_memo_rechecks_live_bytes_rollback_and_repairs_local_state() {
        let root = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(root.path().to_path_buf());
        write_signed_manifest(&paths, fixture_manifest(), TEST_NOW);
        let bytes = fs::read(&paths.manifest).unwrap();
        load_manifest(&paths, TEST_NOW).unwrap();
        fs::remove_file(&paths.last_valid_manifest).unwrap();
        load_manifest(&paths, TEST_NOW).unwrap();
        assert!(paths.last_valid_manifest.is_file());
        write_version_high_water(&paths, 2);
        assert!(matches!(
            load_manifest(&paths, TEST_NOW),
            Err(ManifestProblem::RolledBack { .. })
        ));
        write_version_high_water(&paths, 1);
        let mut changed: SignedManifest = serde_json::from_slice(&bytes).unwrap();
        changed.manifest_bytes.push(' ');
        fs::write(&paths.manifest, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert!(matches!(
            load_manifest(&paths, TEST_NOW),
            Err(ManifestProblem::Invalid(_))
        ));
        fs::remove_file(&paths.manifest).unwrap();
        assert!(matches!(
            load_manifest(&paths, TEST_NOW),
            Err(ManifestProblem::Missing)
        ));
    }

    fn v9_fixture_manifest() -> Manifest {
        serde_json::from_str(include_str!("../tests/fixtures/gh_shim/v9-manifest.json"))
            .expect("v9 manifest fixture")
    }

    fn v10_fixture_manifest() -> Manifest {
        serde_json::from_str(include_str!("../tests/fixtures/gh_shim/v10-manifest.json"))
            .expect("v10 manifest fixture")
    }

    fn v11_fixture_manifest() -> Manifest {
        serde_json::from_str(include_str!("../tests/fixtures/gh_shim/v11-manifest.json"))
            .expect("v11 manifest fixture")
    }

    fn v12_fixture_manifest() -> Manifest {
        serde_json::from_str(include_str!("../tests/fixtures/gh_shim/v12-manifest.json"))
            .expect("v12 manifest fixture")
    }

    fn branch_protection_manifest(method: &str, tier: Tier) -> Manifest {
        let mut manifest = v12_fixture_manifest();
        manifest.manifest_version = 13;
        manifest.api_rules.push(ApiRule {
            method: method.to_string(),
            path_glob: BRANCH_PROTECTION_PATH_GLOB.to_string(),
            tier,
            platform: vec!["macos".to_string(), "linux".to_string()],
            rationale: Some(
                "branch protection is a repository setting; operator identity, audited bypass"
                    .to_string(),
            ),
        });
        manifest
    }

    fn os_args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
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
    fn wrapper_config_dir_patterns_follow_an_absolute_xdg_config_home() {
        let home = Path::new("/home/user");
        let xdg = if cfg!(windows) {
            PathBuf::from("C:\\xdg-config")
        } else {
            PathBuf::from("/xdg-config")
        };
        assert_eq!(
            expand_home_pattern("~/.config/gh-alfonso-*/", Some(home), Some(&xdg)),
            xdg.join("gh-alfonso-*/").to_string_lossy()
        );
        assert_eq!(
            expand_home_pattern(
                "~/.config/gh-alfonso-*/",
                Some(home),
                Some(Path::new("rel"))
            ),
            home.join(".config/gh-alfonso-*/").to_string_lossy(),
            "a relative XDG_CONFIG_HOME is ignored"
        );
        assert_eq!(
            expand_home_pattern("~/.config/gh-alfonso-*/", Some(home), None),
            home.join(".config/gh-alfonso-*/").to_string_lossy()
        );
        assert_eq!(
            expand_home_pattern("~/.gh-wrapper", Some(home), Some(&xdg)),
            home.join(".gh-wrapper").to_string_lossy(),
            "only the ~/.config/ prefix follows XDG_CONFIG_HOME"
        );
    }

    #[test]
    fn state_paths_from_process_obey_the_test_state_guard() {
        let _guard = crate::test_env::gh_shim_state_guard();
        let selected = std::env::var_os(GH_SHIM_STATE_DIR_ENV).expect("test state override");
        assert_eq!(StatePaths::from_process().root, PathBuf::from(selected));
    }

    /// The shim's state ladder is the operator's XDG state home, deliberately
    /// not the daemon's storage root: every governed seat's placed manifest,
    /// high-water and rung cache live there. See `gh_shim_state_dir_from`.
    #[test]
    fn state_dir_uses_dedicated_override_then_xdg_state_home_then_home_and_ignores_empty_values() {
        let xdg_state = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let dedicated = tempfile::tempdir().unwrap();
        let before = fs::metadata(dedicated.path()).unwrap().modified().unwrap();
        let tail = Path::new("cortexkit").join("aft").join("gh-shim");

        assert_eq!(
            gh_shim_state_dir_from(
                Some(dedicated.path().as_os_str()),
                Some(xdg_state.path().as_os_str()),
                Some(home.path().as_os_str()),
            ),
            dedicated.path()
        );
        assert_eq!(
            fs::metadata(dedicated.path()).unwrap().modified().unwrap(),
            before,
            "resolving the dedicated override must not create or rewrite state"
        );
        assert_eq!(
            gh_shim_state_dir_from(
                None,
                Some(xdg_state.path().as_os_str()),
                Some(home.path().as_os_str())
            ),
            xdg_state.path().join(&tail),
            "the operator's XDG state home is the rung the ceremony writes to"
        );
        assert_eq!(
            gh_shim_state_dir_from(
                Some(OsStr::new("")),
                Some(OsStr::new("")),
                Some(home.path().as_os_str())
            ),
            home.path().join(".local/state").join(&tail),
            "empty override and empty XDG_STATE_HOME fall through to HOME"
        );
        assert_eq!(
            gh_shim_state_dir_from(None, Some(OsStr::new("relative/state")), None),
            std::env::temp_dir().join(&tail),
            "a relative XDG_STATE_HOME is not a rung"
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
                "last_probe",
                "bypass_audit",
                "bypass_audit_error",
                "executing_image",
                "executing_image_error",
                "real_gh_resolution",
                "real_gh_resolution_error",
                "manifests_retained",
                "manifests_dir",
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
    fn v11_and_v12_thread_state_manifests_validate() {
        v11_fixture_manifest()
            .validate()
            .expect("v11 keeps the four thread-state verbs at admin without canonicalization");
        v12_fixture_manifest()
            .validate()
            .expect("v12 governed target-and-state tuples must satisfy the canonicalization check");
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

    fn v15_manifest() -> Manifest {
        serde_json::from_str(include_str!("../tests/fixtures/gh_shim/v15-manifest.json"))
            .expect("synthetic v15 manifest fixture")
    }

    #[test]
    fn v15_cancel_bypass_is_admitted_and_audited_before_execution() {
        let _lock = crate::test_env::process_env_lock();
        let _bypass = ScopedTestEnvVar::set("GH_SHIM_BYPASS", Some("operator"));
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let manifest = v15_manifest();
        manifest.validate().unwrap();
        let rung = RungDetermination::r3(TEST_NOW, 15, &test_rung_provenance()).record;
        let binding = AgentBinding {
            repo: "cortexkit/aft".into(),
            agent_id: "alfonso-aft".into(),
        };
        for force in [false, true] {
            let mut args = os_args(&["run", "cancel", "123", "--repo", "cortexkit/aft"]);
            if force {
                args.push("--force".into());
            }
            assert!(
                matches!(classify(&args, &manifest, "macos"), Classification::Admin { tuple } if tuple == "run cancel")
            );
            let status = dispatch_r3(
                &args,
                classify(&args, &manifest, "macos"),
                &manifest,
                &paths,
                &rung,
                &binding,
                TEST_NOW,
                |forwarded| {
                    assert_eq!(forwarded, args);
                    let (records, error) = read_bypass_audit(&paths);
                    assert!(error.is_none());
                    let records = records.unwrap();
                    assert_eq!(records.len(), if force { 2 } else { 1 });
                    assert_eq!(records.last().unwrap()["tuple"], "run cancel");
                    assert_eq!(records.last().unwrap()["repository"], "cortexkit/aft");
                    73
                },
            );
            assert_eq!(status, 73);
        }
    }

    #[test]
    fn v15_cancel_without_bypass_is_admin_refused() {
        let _lock = crate::test_env::process_env_lock();
        let _bypass = ScopedTestEnvVar::set("GH_SHIM_BYPASS", None);
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let manifest = v15_manifest();
        let args = os_args(&["run", "cancel", "123"]);
        assert!(matches!(
            classify(&args, &manifest, "macos"),
            Classification::Admin { .. }
        ));
        let rung = RungDetermination::r3(TEST_NOW, 15, &test_rung_provenance()).record;
        let binding = AgentBinding {
            repo: "cortexkit/aft".into(),
            agent_id: "alfonso-aft".into(),
        };
        assert_eq!(
            dispatch_r3(
                &args,
                classify(&args, &manifest, "macos"),
                &manifest,
                &paths,
                &rung,
                &binding,
                TEST_NOW,
                |_| panic!("cancel without bypass reached upstream")
            ),
            REFUSAL_EXIT_STATUS
        );
        assert!(!paths.bypass_audit.exists());
    }

    #[test]
    fn v15_cancel_unbound_write_requires_audited_bypass() {
        let _lock = crate::test_env::process_env_lock();
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let manifest = v15_manifest();
        let args = os_args(&[
            "run",
            "cancel",
            "123",
            "--repo",
            "unbound/example",
            "--force",
        ]);
        assert!(!is_unbound_safe(&args));
        let write = unbound_write(
            &args,
            &manifest,
            "macos",
            &TargetRepository::from_invocation(&args),
            directory.path(),
        )
        .expect("unbound cancel is a write");
        {
            let _bypass = ScopedTestEnvVar::set("GH_SHIM_BYPASS", None);
            assert_eq!(
                dispatch_unbound_write(&args, &write, &paths, TEST_NOW, |_| panic!(
                    "unbound cancel without bypass reached upstream"
                )),
                REFUSAL_EXIT_STATUS
            );
            assert!(!paths.bypass_audit.exists());
        }
        let _bypass = ScopedTestEnvVar::set("GH_SHIM_BYPASS", Some("operator"));
        assert_eq!(
            dispatch_unbound_write(&args, &write, &paths, TEST_NOW, |forwarded| {
                assert_eq!(forwarded, args);
                let records = read_bypass_audit(&paths).0.unwrap();
                assert_eq!(records.len(), 1);
                assert_eq!(records[0]["tuple"], "run cancel");
                assert_eq!(records[0]["repository"], "unbound/example");
                73
            }),
            73
        );
    }

    #[test]
    fn v15_cancel_preserves_refused_mutation_flags_and_manifest_gate() {
        let mut manifest = v15_manifest();
        for flag in ["--delete-last", "--create-if-none", "--edit-last"] {
            assert!(
                matches!(
                    classify(
                        &os_args(&["run", "cancel", "123", flag]),
                        &manifest,
                        "macos"
                    ),
                    Classification::Unclassified
                ),
                "{flag}"
            );
        }
        let args = os_args(&["run", "cancel", "123", "--force"]);
        manifest.manifest_version = 14;
        assert!(matches!(
            classify(&args, &manifest, "macos"),
            Classification::Unclassified
        ));
        manifest.manifest_version = 15;
        manifest
            .tiers
            .get_mut(&Tier::Admin)
            .unwrap()
            .retain(|decl| decl.tuple() != "run cancel");
        assert!(matches!(
            classify(&args, &manifest, "macos"),
            Classification::Unclassified
        ));
    }

    #[test]
    fn v12_thread_state_verbs_route_target_and_state_and_v11_stays_admin() {
        let v12 = v12_fixture_manifest();
        let v11 = v11_fixture_manifest();
        v12.validate().expect("valid v12 manifest");
        v11.validate().expect("valid v11 manifest");
        assert_eq!(v12.manifest_version, 12);
        assert_eq!(v11.manifest_version, 11);

        let determination =
            RungDetermination::r3(1_700_000_000, v12.manifest_version, &test_rung_provenance());
        let body_file = fixture_dir().join("governed-speech.md");
        let expected_comment = fs::read_to_string(&body_file).expect("speech body fixture");

        let close_args = os_args(&[
            "issue",
            "close",
            "42",
            "--reason",
            "completed",
            "--repo",
            "cortexkit/aft",
        ]);
        let Classification::Governed {
            tuple: close_tuple,
            canonical: close_canonical,
        } = classify(&close_args, &v12, "macos")
        else {
            panic!("v12 issue close must be governed");
        };
        assert_eq!(close_tuple, "issue close");
        assert!(close_canonical
            .argv_forms
            .iter()
            .any(|form| form == TARGET_AND_STATE_FORM));
        let close_request = canonicalize_governed(
            &close_args,
            &close_tuple,
            &close_canonical,
            v12.manifest_version,
        )
        .expect("issue close with --reason should canonicalize");
        let close_wire = governed_wire_request(&determination.record, "alfonso-aft", close_request);
        assert_eq!(close_wire["verb"], "issue close");
        assert_eq!(close_wire["repository"], "cortexkit/aft");
        assert_eq!(close_wire["number"], "42");
        assert_eq!(close_wire["reason"], "completed");
        assert!(close_wire.get("comment").is_none());
        assert!(close_wire.get("action").is_none());
        assert!(close_wire.get("target").is_none());
        assert!(close_wire.get("body").is_none());
        assert!(close_wire.get("delete-branch").is_none());
        assert!(close_wire.get("delete_branch").is_none());

        // Upstream gh's documented spelling "not planned" (with a space) must
        // govern like "not_planned", and the wire must carry the API form.
        for reason_args in [
            vec!["--reason", "not planned"],
            vec!["--reason=not planned"],
            vec!["--reason", "not_planned"],
        ] {
            let mut args = vec!["issue", "close", "42"];
            args.extend(reason_args.iter().copied());
            args.extend(["--repo", "cortexkit/aft"]);
            let args = os_args(&args);
            let Classification::Governed { tuple, canonical } = classify(&args, &v12, "macos")
            else {
                panic!("v12 issue close {reason_args:?} must be governed");
            };
            let request = canonicalize_governed(&args, &tuple, &canonical, v12.manifest_version)
                .unwrap_or_else(|error| {
                    panic!("issue close {reason_args:?} must canonicalize: {error:?}")
                });
            let wire = governed_wire_request(&determination.record, "alfonso-aft", request);
            assert_eq!(wire["reason"], "not_planned", "{reason_args:?}");
        }

        let reopen_args = os_args(&["pr", "reopen", "7", "--repo", "cortexkit/aft"]);
        let Classification::Governed {
            tuple: reopen_tuple,
            canonical: reopen_canonical,
        } = classify(&reopen_args, &v12, "macos")
        else {
            panic!("v12 pr reopen must be governed");
        };
        assert_eq!(reopen_tuple, "pr reopen");
        let reopen_request = canonicalize_governed(
            &reopen_args,
            &reopen_tuple,
            &reopen_canonical,
            v12.manifest_version,
        )
        .expect("pr reopen should canonicalize without --reason");
        let reopen_wire =
            governed_wire_request(&determination.record, "alfonso-aft", reopen_request);
        assert_eq!(reopen_wire["verb"], "pr reopen");
        assert_eq!(reopen_wire["repository"], "cortexkit/aft");
        assert_eq!(reopen_wire["number"], "7");
        assert!(reopen_wire.get("reason").is_none());
        assert!(reopen_wire.get("comment").is_none());
        assert!(reopen_wire.get("delete-branch").is_none());

        for (args, expected_tuple) in [
            (
                os_args(&["issue", "close", "42", "--reason", "not_planned"]),
                "issue close",
            ),
            (os_args(&["issue", "reopen", "42"]), "issue reopen"),
            (os_args(&["pr", "close", "7"]), "pr close"),
            (os_args(&["pr", "reopen", "7"]), "pr reopen"),
        ] {
            assert!(
                matches!(
                    classify(&args, &v12, "macos"),
                    Classification::Governed { ref tuple, .. } if tuple == expected_tuple
                ),
                "v12 must govern {expected_tuple}"
            );
            assert!(
                matches!(
                    classify(&args, &v11, "macos"),
                    Classification::Admin { ref tuple } if tuple == expected_tuple
                ),
                "v11 must keep {expected_tuple} on the admin tier"
            );
        }

        let missing_reason = os_args(&["issue", "close", "42", "--repo", "cortexkit/aft"]);
        let Classification::Governed { tuple, canonical } =
            classify(&missing_reason, &v12, "macos")
        else {
            panic!("missing --reason is still the governed issue close tuple");
        };
        let missing =
            canonicalize_governed(&missing_reason, &tuple, &canonical, v12.manifest_version)
                .expect_err("issue close without --reason must refuse");
        assert_eq!(missing.code, RefusalCode::MissingReason);
        assert_eq!(missing.code.as_str(), "gh_shim_missing_reason");
        assert!(
            missing.contains("--reason"),
            "missing-reason refusal must name --reason: {missing}"
        );
        assert_eq!(
            refuse_governed_canonicalization(&missing),
            REFUSAL_EXIT_STATUS
        );

        for flag in ["--delete-branch", "-d"] {
            let args = os_args(&["pr", "close", "7", flag, "--repo", "cortexkit/aft"]);
            let Classification::Governed { tuple, canonical } = classify(&args, &v12, "macos")
            else {
                panic!("pr close with {flag} must still classify as governed");
            };
            let error = canonicalize_governed(&args, &tuple, &canonical, v12.manifest_version)
                .expect_err("pr close {flag} must refuse before routing");
            assert_eq!(error.code, RefusalCode::DestructiveFlag);
            assert_eq!(error.code.as_str(), "gh_shim_destructive_flag");
            assert!(
                error.contains(flag),
                "destructive refusal must name {flag}: {error}"
            );
            assert!(
                error.contains("branch deletion stays undeclared"),
                "destructive refusal must say branch deletion stays undeclared: {error}"
            );
            assert_eq!(
                refuse_governed_canonicalization(&error),
                REFUSAL_EXIT_STATUS
            );
        }

        let reason_on_reopen = os_args(&[
            "issue",
            "reopen",
            "42",
            "--reason",
            "completed",
            "--repo",
            "cortexkit/aft",
        ]);
        let Classification::Governed { tuple, canonical } =
            classify(&reason_on_reopen, &v12, "macos")
        else {
            panic!("issue reopen remains governed");
        };
        let rejected_reason =
            canonicalize_governed(&reason_on_reopen, &tuple, &canonical, v12.manifest_version)
                .expect_err("--reason is issue close only");
        assert_eq!(rejected_reason.code, RefusalCode::Unclassified);
        assert!(rejected_reason.contains("--reason"));

        let inline_comment = os_args(&[
            "issue",
            "close",
            "42",
            "--reason",
            "completed",
            "--comment",
            "Closing as done.",
            "--repo",
            "cortexkit/aft",
        ]);
        let Classification::Governed { tuple, canonical } =
            classify(&inline_comment, &v12, "macos")
        else {
            panic!("issue close with --comment must be governed");
        };
        let commented =
            canonicalize_governed(&inline_comment, &tuple, &canonical, v12.manifest_version)
                .expect("--comment should canonicalize");
        assert_eq!(commented.body["comment"], "Closing as done.");
        let commented_wire = governed_wire_request(&determination.record, "alfonso-aft", commented);
        assert_eq!(commented_wire["comment"], "Closing as done.");

        let short_comment = os_args(&[
            "pr",
            "close",
            "7",
            "-c",
            "Closing the pull request.",
            "--repo",
            "cortexkit/aft",
        ]);
        let Classification::Governed { tuple, canonical } = classify(&short_comment, &v12, "macos")
        else {
            panic!("pr close with -c must be governed");
        };
        let short = canonicalize_governed(&short_comment, &tuple, &canonical, v12.manifest_version)
            .expect("-c should canonicalize");
        assert_eq!(short.body["comment"], "Closing the pull request.");

        let file_comment = os_args(&[
            "issue",
            "reopen",
            "42",
            "--comment-file",
            body_file.to_str().expect("utf-8 body file path"),
            "--repo",
            "cortexkit/aft",
        ]);
        let Classification::Governed { tuple, canonical } = classify(&file_comment, &v12, "macos")
        else {
            panic!("issue reopen with --comment-file must be governed");
        };
        let from_file =
            canonicalize_governed(&file_comment, &tuple, &canonical, v12.manifest_version)
                .expect("--comment-file should reuse body-file plumbing");
        assert_eq!(from_file.body["comment"], expected_comment);

        let mut stdin = std::io::Cursor::new("comment supplied through stdin");
        assert_eq!(
            read_body_file_from(Path::new("-"), &mut stdin).unwrap(),
            "comment supplied through stdin"
        );
    }

    #[test]
    fn thread_state_holder_applied_and_partial_comment_outcomes_render_returned_state() {
        let applied_close = json!({
            "outcome": "applied",
            "state": "closed",
            "state_reason": "not_planned"
        });
        let applied = parse_governed_response(&serde_json::to_vec(&applied_close).unwrap())
            .expect("applied close should parse");
        let RouteOutcome::Result(text) = applied else {
            panic!("applied close must be a successful result, got {applied:?}");
        };
        assert!(
            text.contains("not_planned"),
            "returned state_reason must be printed: {text:?}"
        );
        assert!(
            !text.contains("completed"),
            "request reason must not be echoed: {text:?}"
        );
        assert!(
            text.contains("closed"),
            "returned state must be printed: {text:?}"
        );

        let partial = json!({
            "outcome": "state_applied_comment_failed",
            "state": "closed",
            "state_reason": "completed",
            "comment_error": {
                "code": "rate_limited",
                "detail": "secondary rate limit on issue comments"
            }
        });
        let partial_outcome = parse_governed_response(&serde_json::to_vec(&partial).unwrap())
            .expect("partial comment failure should parse");
        let RouteOutcome::StateAppliedCommentFailed(partial_text) = &partial_outcome else {
            panic!("partial must not be a seam refusal or full success, got {partial_outcome:?}");
        };
        assert!(
            partial_text.contains("APPLIED"),
            "partial must print an APPLIED line: {partial_text:?}"
        );
        assert!(
            partial_text.contains("rate_limited"),
            "partial must print comment_error code: {partial_text:?}"
        );
        assert!(
            partial_text.contains("secondary rate limit on issue comments"),
            "partial must print comment_error detail: {partial_text:?}"
        );
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let binding = AgentBinding {
            repo: "owner/repo".to_string(),
            agent_id: "agent-7".to_string(),
        };
        assert_eq!(
            governed_outcome_status(&paths, &binding, 123, partial_outcome),
            UPSTREAM_FAILURE_EXIT_STATUS
        );

        let applied_reopen = json!({
            "outcome": "applied",
            "state": "open"
        });
        let reopen = parse_governed_response(&serde_json::to_vec(&applied_reopen).unwrap())
            .expect("applied reopen should parse");
        let RouteOutcome::Result(reopen_text) = reopen else {
            panic!("applied reopen must be a successful result, got {reopen:?}");
        };
        assert!(
            reopen_text.contains("open"),
            "reopen must print returned state: {reopen_text:?}"
        );
        assert_eq!(
            governed_outcome_status(&paths, &binding, 123, RouteOutcome::Result(reopen_text)),
            0
        );
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
        // The command names no repository, so the request takes it from
        // GH_REPO, else from the working directory's git origin. Pin GH_REPO
        // so the result does not depend on the checkout: a checkout without
        // a github.com origin (a CI or Windows gate copy) would otherwise
        // send no repository at all. The name deliberately differs from the
        // producer's, the way a fork's origin would.
        const CONSUMER_REPOSITORY: &str = "consumer-fork/aft";
        let _env_lock = crate::test_env::process_env_lock();
        let _gh_repo = ScopedTestEnvVar::set("GH_REPO", Some(CONSUMER_REPOSITORY));
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
        expected["metadata"]["pid"] = json!(std::process::id());
        expected["metadata"]
            .as_object_mut()
            .expect("expected metadata object")
            .remove("agent_id");
        let mut actual = wire;
        // The repository value is the consumer's own (GH_REPO above), so it
        // intentionally differs from the producer's. Its presence and
        // owner/name format still belong to the producer's request contract,
        // so require exactly the pinned name before copying it into the
        // expected request; a missing or malformed value fails here.
        assert_eq!(
            actual["repository"], CONSUMER_REPOSITORY,
            "wire request must carry the GH_REPO repository as owner/name"
        );
        expected["repository"] = actual["repository"].clone();
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
    fn manifest_rejects_api_rules_for_unknown_host_platforms() {
        let mut manifest = branch_protection_manifest("PUT", Tier::Admin);
        manifest
            .api_rules
            .last_mut()
            .expect("branch protection API rule")
            .platform = vec!["github".to_string()];
        assert_eq!(
            manifest.validate().unwrap_err(),
            "api rule PUT /repos/*/*/branches/*/protection names unknown host platform github"
        );
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
            author_scope: None,
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

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
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
    fn read_only_cli_actions_are_mechanical_and_delegate_on_a_bound_v13_manifest() {
        use std::cell::Cell;

        let directory = tempfile::tempdir().expect("create read-only dispatch state directory");
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let mut manifest = v13_manifest();
        // Clear the manifest's declared reads so each case verifies that read
        // access comes from the code-defined allowlist, not a manifest tier row.
        manifest
            .tiers
            .get_mut(&Tier::Mechanical)
            .expect("v13 mechanical tier")
            .clear();
        let rung =
            RungDetermination::r3(TEST_NOW, manifest.manifest_version, &test_rung_provenance())
                .record;
        let binding = AgentBinding {
            repo: "cortexkit/aft".to_string(),
            agent_id: manifest
                .bindings
                .get("cortexkit/aft")
                .expect("bound v13 repository")
                .clone(),
        };
        let cases: &[(&str, &[&str])] = &[
            ("issue view", &["issue", "view", "319", "--comments"]),
            ("issue list", &["issue", "list"]),
            ("issue status", &["issue", "status"]),
            ("pr view", &["pr", "view", "313", "--web"]),
            ("pr list", &["pr", "list"]),
            ("pr status", &["pr", "status"]),
            ("pr checks", &["pr", "checks", "313", "--watch"]),
            ("pr diff", &["pr", "diff", "313"]),
            ("pr diff", &["pr", "diff", "313", "--repo", "cortexkit/aft"]),
            ("release view", &["release", "view", "v0.56.2"]),
            ("release list", &["release", "list"]),
            (
                "release download",
                &["release", "download", "v0.56.2", "--dir", "artifacts"],
            ),
            ("repo view", &["repo", "view"]),
            ("repo list", &["repo", "list"]),
            ("run view", &["run", "view", "123"]),
            ("run list", &["run", "list"]),
            ("run watch", &["run", "watch", "123"]),
            (
                "run download",
                &["run", "download", "123", "--dir", "artifacts"],
            ),
            ("workflow view", &["workflow", "view", "ci.yml"]),
            ("workflow list", &["workflow", "list"]),
            ("label list", &["label", "list"]),
            ("search issues", &["search", "issues", "routing shim"]),
            ("search prs", &["search", "prs", "routing shim"]),
            ("search repos", &["search", "repos", "cortexkit"]),
            (
                "search code",
                &["search", "code", "READ_ONLY_ACTION_TUPLES"],
            ),
            ("search commits", &["search", "commits", "read-only gh"]),
            ("cache list", &["cache", "list"]),
        ];

        for (tuple, raw_args) in cases {
            let args = os_args(raw_args);
            let classification = classify(&args, &manifest, "macos");
            assert!(
                matches!(&classification, Classification::Mechanical),
                "expected classification-only read passthrough for {tuple}: {classification:?}"
            );

            let delegated = Cell::new(0);
            let status = dispatch_r3(
                &args,
                classification,
                &manifest,
                &paths,
                &rung,
                &binding,
                TEST_NOW,
                |delegated_args| {
                    assert_eq!(delegated_args, args);
                    delegated.set(delegated.get() + 1);
                    73
                },
            );
            assert_eq!(status, 73, "upstream status for {tuple}");
            assert_eq!(delegated.get(), 1, "upstream delegation count for {tuple}");
        }

        for (raw_args, expected) in [
            (&["issue", "close", "319"][..], "governed"),
            (&["pr", "merge", "313"][..], "admin"),
            (&["release", "delete", "v0.56.2"][..], "destructive"),
            (
                &["release", "upload", "v0.56.2", "artifact.tar.gz"][..],
                "admin",
            ),
            (
                &["issue", "comment", "319", "--body", "hello"][..],
                "governed",
            ),
        ] {
            let classification = classify(&os_args(raw_args), &manifest, "macos");
            let matches_expected = match expected {
                "governed" => matches!(&classification, Classification::Governed { .. }),
                "admin" => matches!(&classification, Classification::Admin { .. }),
                "destructive" => matches!(&classification, Classification::Destructive),
                _ => unreachable!("unknown expected classification"),
            };
            assert!(
                matches_expected,
                "negative control {raw_args:?} changed from {expected}: {classification:?}"
            );
        }
    }

    const GRAPHQL_READ_DOCUMENTS: &[&str] = &[
        r#"{ repository(owner:"o", name:"r") { pullRequest(number:1) { reviewThreads(first:100){nodes{isResolved}} } } }"#,
        "query Read($n: Int!) { repository { issue(number: $n) { title } } }",
        "query First { viewer { login } } query Second { viewer { id } }",
        r#"{ repository(name:"mutation") { mutationField } }"#,
        "# mutation is only a comment\n{ viewer { login } }",
        r#"query Read @skip(if: false) { field(arg: "\"}") }"#,
        "{ field(arg: \"\"\"mutation { text }\"\"\") }",
        "query Read { viewer { ...Identity } } fragment Identity on User { login }",
        "fragment Identity on User { login } { viewer { ...Identity } }",
    ];

    #[test]
    fn graphql_reads_delegate_unchanged_as_mechanical_operator_reads() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let manifest = fixture_manifest();
        let rung =
            RungDetermination::r3(TEST_NOW, manifest.manifest_version, &test_rung_provenance())
                .record;
        let binding = AgentBinding {
            repo: "cortexkit/aft".into(),
            agent_id: "bound-agent".into(),
        };
        for query in GRAPHQL_READ_DOCUMENTS {
            let args = os_args(&[
                "api",
                "graphql",
                "-f",
                &format!("query={query}"),
                "-F",
                "n=1",
            ]);
            let classification = classify(&args, &manifest, "macos");
            assert!(
                matches!(classification, Classification::Mechanical),
                "{query}: {classification:?}"
            );
            assert!(!api_invocation_writes(&args));
            assert_eq!(
                dispatch_r3(
                    &args,
                    classification,
                    &manifest,
                    &paths,
                    &rung,
                    &binding,
                    TEST_NOW,
                    |forwarded| {
                        assert_eq!(forwarded, args);
                        73
                    }
                ),
                73
            );
            assert!(!paths.seam_state.exists());
            assert!(!paths.bypass_audit.exists());
        }
    }

    #[test]
    fn graphql_mutations_subscriptions_and_uninspectable_queries_are_refused() {
        let manifest = fixture_manifest();
        for tail in [
            &[][..],
            &["-f", "query=mutation { addStar }"],
            &["-f", "query=subscription { events }"],
            &[
                "-f",
                "query=query Read { viewer { login } } mutation Write { addStar }",
                "-f",
                "operationName=Write",
            ],
            &["-F", "query=@-"],
            &["-f", "query=@file"],
            &["--input", "body.json", "-f", "query={ viewer { login } }"],
            &["--input=-", "-f", "query={ viewer { login } }"],
            &[
                "-f",
                "query={ viewer { login } }",
                "-f",
                "query=mutation { addStar }",
            ],
            &[
                "-f",
                "query={ viewer { login } }",
                "-f",
                "query[]=mutation { addStar }",
            ],
            &["-f", "query={ viewer { login }"],
            &["-f", "query=not_a_document"],
            &["-f", "query={ field(arg: \"unterminated) }"],
            &["-f", "query={ field(arg: [1, 2) }"],
            &[
                "-f",
                "query={ field(arg: \"\"\"text\"\"\") } mutation { addStar }",
            ],
            &[
                "-f",
                "query=query Read($n: Int!) { viewer { login } } subscription Watch { events }",
            ],
            &["-f", "query={ viewer { login } }", "--unknown"],
            &["-X", "GET", "-f", "query=mutation { addStar }"],
        ] {
            let mut args = os_args(&["api", "graphql"]);
            args.extend(os_args(tail));
            assert!(!graphql_query_is_read_only(&args), "{tail:?}");
            assert!(api_invocation_writes(&args), "{tail:?}");
            assert!(
                matches!(
                    classify(&args, &manifest, "macos"),
                    Classification::Unclassified
                ),
                "{tail:?}"
            );
        }
    }

    #[test]
    fn graphql_block_string_escape_cannot_hide_a_selected_mutation() {
        let document = r#"{ f(a: """\\"""X""") } mutation Write { addStar(input:{starrableId:"x"}) { clientMutationId } } # """ ) }"#;
        let args = os_args(&[
            "api",
            "graphql",
            "-f",
            &format!("query={document}"),
            "-f",
            "operationName=Write",
        ]);
        assert!(
            !graphql_query_is_read_only(&args),
            "selected mutation must never be admitted: {document}"
        );
        assert!(api_invocation_writes(&args));
        assert!(matches!(
            classify(&args, &fixture_manifest(), "macos"),
            Classification::Unclassified
        ));
        assert_eq!(api_refusal_subject(&args), "api graphql (mutation)");
    }

    const GRAPHQL_STRING_VALUES: &[&str] = &[
        r##""plain""##,
        r##""\" mutation Write { addStar } #""##,
        r##""# mutation is string text""##,
        r##""\\\"""##,
        r##""\n\r\t\u0022""##,
        r##""""block # mutation { addStar }""""##,
        r##""""\""" mutation { addStar } #""""##,
        r##""""\\"""X""""##,
        "\"\"\"literal \\n and \\q\r\n# still a string\"\"\"",
    ];

    #[test]
    fn graphql_parser_distinguishes_strings_comments_and_operations() {
        let manifest = fixture_manifest();
        for prefix in ["", "\u{feff}", "# ignored mutation { addStar }\r\n"] {
            for value in GRAPHQL_STRING_VALUES {
                let query = format!("{prefix}query Read {{ field(arg: {value}) }}");
                let args = os_args(&["api", "graphql", "-f", &format!("query={query}")]);
                assert!(graphql_query_is_read_only(&args), "{query}");
                assert!(matches!(
                    classify(&args, &manifest, "macos"),
                    Classification::Mechanical
                ));
                assert_eq!(
                    api_refusal_subject(&args),
                    "api graphql (uninspectable query)",
                    "{query}"
                );

                // Even selecting the query must not authorize another operation
                // in the same document to run with the operator's credentials.
                let mixed = format!("{query} # comment\r\nmutation Write {{ addStar }}");
                for selected in ["Read", "Write"] {
                    let args = os_args(&[
                        "api",
                        "graphql",
                        "-f",
                        &format!("query={mixed}"),
                        "-f",
                        &format!("operationName={selected}"),
                    ]);
                    assert!(!graphql_query_is_read_only(&args), "{mixed}");
                    assert!(api_invocation_writes(&args));
                    assert!(matches!(
                        classify(&args, &manifest, "macos"),
                        Classification::Unclassified
                    ));
                    assert_eq!(api_refusal_subject(&args), "api graphql (mutation)");
                }
            }
        }
    }

    const GRAPHQL_REFUSED_DOCUMENTS: &[&str] = &[
        "",
        "# no operation\r\n",
        "mutation { addStar }",
        "subscription { events }",
        "query Read { viewer { login } } mutation Write { addStar }",
        "query Read($n: Int!) { viewer { login } } subscription Watch { events }",
        "@-",
        "@file",
        "{ viewer { login }",
        "not_a_document",
        r#"{ field(arg: "unterminated) }"#,
        "{ field(arg: [1, 2) }",
        r#"{ field(arg: """text""") } mutation { addStar }"#,
        "fragment Only on Query { viewer { login } }",
        "fragment Only on Mutation { addStar }",
        "fragment Only on Query { viewer { login } } mutation Write { addStar }",
        "fragment Only on Query { mutation Write { addStar } }",
        "{}",
        "query Read($n Int) { viewer }",
        "{ field(arg: 01) }",
        "{ field(arg: 1.) }",
        r#"{ field(arg: "\q") }"#,
        r#"{ field(arg: "\uZZZZ") }"#,
        r#"{ field(arg: """unterminated) }"#,
    ];

    const GRAPHQL_TYPE_SYSTEM_DOCUMENTS: &[&str] = &[
        "schema { query: Query }",
        "scalar Custom",
        "type Query { viewer: User }",
        "interface Node { id: ID! }",
        "union Result = User | Issue",
        "enum Color { RED GREEN }",
        "input Filter { name: String }",
        "directive @custom on FIELD",
        "extend schema { mutation: Mutation }",
        "extend scalar Custom @custom",
        "extend type Query { other: String }",
        "extend interface Node { name: String }",
        "extend union Result = PullRequest",
        "extend enum Color { BLUE }",
        "extend input Filter { id: ID }",
    ];

    #[test]
    fn graphql_parser_refuses_non_query_definitions_and_parse_errors() {
        let manifest = fixture_manifest();
        for document in GRAPHQL_REFUSED_DOCUMENTS {
            let args = os_args(&["api", "graphql", "-f", &format!("query={document}")]);
            assert!(!graphql_query_is_read_only(&args), "{document}");
            assert!(api_invocation_writes(&args));
            assert!(matches!(
                classify(&args, &manifest, "macos"),
                Classification::Unclassified
            ));
        }
        for definition in GRAPHQL_TYPE_SYSTEM_DOCUMENTS {
            for prefix in ["", "query Read { viewer { login } } "] {
                let document = format!("{prefix}{definition}");
                let parsed = apollo_parser::Parser::new(&document).parse();
                assert_eq!(parsed.errors().count(), 0, "{document}");
                assert!(!graphql_document_is_read_only(&document), "{document}");
            }
        }
        for document in [
            "fragment Only on Query { mutation }",
            "type Mutation { addStar: String }",
            "query { mutation }",
            "mutation Write { addStar } ?",
            "query { viewer } # mutation Write { addStar }",
        ] {
            let args = os_args(&["api", "graphql", "-f", &format!("query={document}")]);
            assert_eq!(
                api_refusal_subject(&args),
                "api graphql (uninspectable query)",
                "{document}"
            );
        }
    }

    #[test]
    fn graphql_admission_matches_an_independent_full_parse_over_a_corpus() {
        use graphql_parser::query::{Definition, OperationDefinition};

        let mut corpus: Vec<String> = GRAPHQL_READ_DOCUMENTS
            .iter()
            .chain(GRAPHQL_REFUSED_DOCUMENTS)
            .chain(GRAPHQL_TYPE_SYSTEM_DOCUMENTS)
            .map(|document| (*document).to_string())
            .collect();
        corpus.push(r#"{ f(a: """\\"""X""") } mutation Write { addStar(input:{starrableId:"x"}) { clientMutationId } } # """ ) }"#.into());
        for prefix in ["", "\u{feff}", "# mutation { ignored }\r\n"] {
            for value in GRAPHQL_STRING_VALUES {
                for operation in ["query Read", "mutation Write", "subscription Watch", ""] {
                    for suffix in [
                        "",
                        " # mutation { ignored }\r\n",
                        "\r\nquery Another { viewer { login } }",
                        "\r\nmutation WriteAgain { addStar }",
                        "\r\nsubscription WatchAgain { events }",
                        "\r\nfragment Fields on Query { viewer { login } }",
                        "\r\ntype Query { field: String }",
                        "\r\n{ field(arg: \"\\q\") }",
                    ] {
                        corpus.push(format!(
                            "{prefix}{operation} {{ field(arg: {value}) }}{suffix}"
                        ));
                    }
                }
            }
        }

        let mut admitted = 0;
        let mut refused = 0;
        for document in &corpus {
            let args = os_args(&["api", "graphql", "-f", &format!("query={document}")]);
            if !graphql_query_is_read_only(&args) {
                refused += 1;
                continue;
            }
            admitted += 1;
            // Use a second implementation, not the admission summary, to check
            // the actual document's full grammar and operation kinds.
            let parsed = graphql_parser::parse_query::<String>(document)
                .unwrap_or_else(|error| panic!("admitted invalid document {document:?}: {error}"));
            let mut operations = 0;
            for definition in parsed.definitions {
                match definition {
                    Definition::Operation(OperationDefinition::Query(_))
                    | Definition::Operation(OperationDefinition::SelectionSet(_)) => {
                        operations += 1
                    }
                    Definition::Fragment(_) => {}
                    other => panic!("admitted non-query definition in {document:?}: {other:?}"),
                }
            }
            assert!(
                operations > 0,
                "admitted fragment-only document: {document}"
            );
        }
        assert!(admitted > 0 && refused > 0);
        assert!(corpus.len() > 800, "generated corpus did not run");
    }

    #[test]
    fn graphql_magic_query_placeholders_are_uninspectable() {
        let manifest = fixture_manifest();
        for placeholder in ["{owner}", "{repo}", "{branch}"] {
            let query = format!("query={{ field(arg: \"{placeholder}\") }}");
            for tail in [
                vec!["-F".to_string(), query.clone()],
                vec!["--field".to_string(), query.clone()],
                vec![format!("--field={query}")],
                vec![format!("-F{query}")],
            ] {
                let mut args = os_args(&["api", "graphql"]);
                args.extend(tail.iter().map(OsString::from));
                assert!(!graphql_query_is_read_only(&args), "{tail:?}");
                assert!(matches!(
                    classify(&args, &manifest, "macos"),
                    Classification::Unclassified
                ));
            }
            // Raw fields are literal: gh does not expand their placeholders.
            assert!(graphql_query_is_read_only(&os_args(&[
                "api", "graphql", "-f", &query
            ])));
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
        assert_eq!(RefusalCode::ALL.len(), 17);
        assert!(RefusalCode::ALL
            .iter()
            .all(|code| code.as_str().starts_with("gh_shim_")));
        assert_eq!(
            RefusalCode::GovernanceUnavailable.as_str(),
            "gh_shim_governance_unavailable"
        );
        assert_eq!(
            RefusalCode::OutcomeUnknown.as_str(),
            "gh_shim_outcome_unknown"
        );
        assert_eq!(
            RefusalCode::UnsupportedFlag.as_str(),
            "gh_shim_unsupported_flag"
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
        assert_eq!(OUTCOME_UNKNOWN_EXIT_STATUS, 87);
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
    fn v13_release_maintenance_rows_allow_reviewed_flags_and_refuse_destructive_forms() {
        // The v13 shape is built here rather than read from a ceremony file:
        // the signed payload lives in the operator's state directory and the
        // assembled draft under the gitignored `.alfonso/`, so neither exists
        // on a clean checkout. This is the classifier's view of v13 - the
        // admin release rows plus the branch-protection API rules.
        let mut manifest = v13_manifest();
        manifest.api_rules.push(ApiRule {
            method: "DELETE".to_string(),
            path_glob: BRANCH_PROTECTION_PATH_GLOB.to_string(),
            tier: Tier::Admin,
            platform: vec!["macos".to_string(), "linux".to_string()],
            rationale: Some(
                "branch protection is a repository setting; operator identity, audited bypass"
                    .to_string(),
            ),
        });
        manifest.validate().expect("valid v13 admin extensions");
        for method in ["PUT", "DELETE"] {
            let rule = manifest
                .api_rules
                .iter()
                .find(|rule| rule.method == method && rule.path_glob == BRANCH_PROTECTION_PATH_GLOB)
                .expect("v13 branch protection API rule");
            assert_eq!(rule.tier, Tier::Admin);
            assert_eq!(rule.platform, ["macos", "linux"]);
            assert_eq!(
                rule.rationale.as_deref(),
                Some(
                    "branch protection is a repository setting; operator identity, audited bypass"
                )
            );
        }

        for args in [
            vec![
                "release",
                "edit",
                "v1.2.3",
                "--notes",
                "notes",
                "--notes-file",
                "notes.md",
                "--title",
                "Dashboard",
                "--draft=false",
                "--latest",
                "--prerelease",
            ],
            vec!["release", "upload", "v1.2.3", "dashboard.json", "--clobber"],
        ] {
            let args = os_args(&args);
            assert!(matches!(
                classify(&args, &manifest, "macos"),
                Classification::Admin { ref tuple }
                    if tuple == if args[1] == "edit" { "release edit" } else { "release upload" }
            ));
        }
        assert!(is_reviewed_admin_tuple(13, "release edit"));
        assert!(is_reviewed_admin_tuple(13, "release upload"));
        assert!(!is_reviewed_admin_tuple(12, "release edit"));
        assert!(!is_reviewed_admin_tuple(12, "release upload"));

        for args in [
            os_args(&["release", "delete", "v1.2.3"]),
            os_args(&["release", "delete-asset", "v1.2.3", "dashboard.json"]),
            os_args(&["release", "edit", "v1.2.3", "--delete-tag"]),
            os_args(&["release", "upload", "v1.2.3", "--delete-asset"]),
        ] {
            assert!(matches!(
                classify(&args, &manifest, "macos"),
                Classification::Destructive
            ));
            assert_eq!(
                RefusalCode::DestructiveFlag.as_str(),
                "gh_shim_destructive_flag"
            );
            assert_eq!(
                refuse(
                    RefusalCode::DestructiveFlag,
                    "destructive GitHub operations are not available through the shim"
                ),
                REFUSAL_EXIT_STATUS
            );
        }
    }

    /// The classifier's view of v14: the v12 fixture plus the speech rows the
    /// v14 payload adds. Built here rather than read from the assembled
    /// payload, which lives under the gitignored `.alfonso/` and is absent on a
    /// clean checkout.
    fn v14_manifest() -> Manifest {
        let mut manifest = v12_fixture_manifest();
        manifest.manifest_version = 14;
        manifest
            .tiers
            .get_mut(&Tier::Governed)
            .expect("v14 governed tier")
            .push(TupleDecl::Details {
                tuple: "issue create".to_string(),
                platform: vec!["macos".to_string(), "linux".to_string()],
                api_match: None,
                rationale: Some(
                    "Speech tier, the same authority class as issue comment and issue close"
                        .to_string(),
                ),
            });
        manifest
            .tiers
            .get_mut(&Tier::Governed)
            .expect("v14 governed tier")
            .push(TupleDecl::Details {
                tuple: "issue edit".to_string(),
                platform: vec!["macos".to_string(), "linux".to_string()],
                api_match: None,
                rationale: Some(
                    "Own-issue edit: public speech; the holder verifies authorship".to_string(),
                ),
            });
        manifest.canonicalization.insert(
            "issue create".to_string(),
            Canonicalization {
                argv_forms: vec![FIELDS_ONLY_FORM.to_string()],
                target_fields: Vec::new(),
                body_fields: vec![
                    "title".to_string(),
                    "body".to_string(),
                    "labels".to_string(),
                ],
            },
        );
        manifest.canonicalization.insert(
            "issue edit".to_string(),
            Canonicalization {
                argv_forms: vec!["target-and-fields".to_string()],
                target_fields: vec!["number".to_string()],
                body_fields: vec![
                    "title".to_string(),
                    "body".to_string(),
                    "add_labels".to_string(),
                    "remove_labels".to_string(),
                    "add_assignees".to_string(),
                    "remove_assignees".to_string(),
                ],
            },
        );
        manifest.api_rules.push(ApiRule {
            method: OWN_COMMENT_PATCH_METHOD.to_string(),
            path_glob: OWN_COMMENT_PATCH_PATH_GLOB.to_string(),
            tier: Tier::Governed,
            platform: vec!["macos".to_string(), "linux".to_string()],
            rationale: Some(
                "Own-comment edit: body-only speech; the holder verifies authorship".to_string(),
            ),
        });
        let admin = manifest
            .tiers
            .get_mut(&Tier::Admin)
            .expect("v14 admin tier");
        for tuple in ["pr edit", "label create"] {
            admin.push(TupleDecl::Details {
                tuple: tuple.to_string(),
                platform: vec!["macos".to_string(), "linux".to_string()],
                api_match: None,
                rationale: Some(
                    "Operator label row: label-only pr edit and label create, audited".to_string(),
                ),
            });
        }
        manifest
    }

    /// The v13 shape as it is deployed today, for the two-way comparisons the
    /// v14 rows have to survive.
    fn v13_manifest() -> Manifest {
        let mut manifest = branch_protection_manifest("PUT", Tier::Admin);
        let admin = manifest
            .tiers
            .get_mut(&Tier::Admin)
            .expect("v13 admin tier");
        for tuple in V13_ADMIN_TUPLES {
            admin.push(TupleDecl::Details {
                tuple: (*tuple).to_string(),
                platform: vec!["macos".to_string(), "linux".to_string()],
                api_match: None,
                rationale: None,
            });
        }
        manifest
    }

    #[test]
    fn v14_issue_create_is_bot_speech_and_v13_leaves_it_undeclared() {
        let manifest = v14_manifest();
        manifest.validate().expect("valid v14 speech extensions");
        assert!(is_reviewed_governed_tuple(14, "issue create"));
        assert!(!is_reviewed_governed_tuple(13, "issue create"));

        let args = os_args(&["issue", "create", "--title", "Filing", "--body", "Prose"]);
        let Classification::Governed { tuple, canonical } = classify(&args, &manifest, "macos")
        else {
            panic!("v14 issue create must be governed bot speech");
        };
        assert_eq!(tuple, "issue create");
        let request = canonicalize_governed(&args, &tuple, &canonical, manifest.manifest_version)
            .expect("issue create canonicalizes");
        assert_eq!(request.action, "issue create");
        // A create names no target: the issue has no number until it exists.
        assert!(request.target.is_empty());
        assert_eq!(request.body["title"], json!("Filing"));
        assert_eq!(request.body["body"], json!("Prose"));

        // The same argv under the deployed v13 shape is undeclared.
        assert!(matches!(
            classify(&args, &v13_manifest(), "macos"),
            Classification::Unclassified
        ));
    }

    #[test]
    fn v14_issue_create_admits_repeated_labels_and_leaves_a_missing_title_to_upstream() {
        let manifest = v14_manifest();
        let args = os_args(&[
            "issue",
            "create",
            "--title",
            "Filing",
            "--body-file",
            "-",
            "--label",
            "bug",
            "--label=p1",
            "--repo",
            "cortexkit/aft",
        ]);
        let Classification::Governed { tuple, canonical } = classify(&args, &manifest, "macos")
        else {
            panic!("labelled issue create must be governed");
        };
        let request = canonicalize_governed(&args, &tuple, &canonical, manifest.manifest_version)
            .expect("labelled issue create canonicalizes");
        assert_eq!(request.body["labels"], json!(["bug", "p1"]));
        assert_eq!(request.repository.as_deref(), Some("cortexkit/aft"));

        // `gh issue create` without --title already fails with upstream's own
        // error text, so the shim must not invent a second refusal for it.
        let no_title = os_args(&["issue", "create", "--body", "Prose"]);
        let Classification::Governed { tuple, canonical } = classify(&no_title, &manifest, "macos")
        else {
            panic!("a title-less issue create is still the declared verb");
        };
        let request =
            canonicalize_governed(&no_title, &tuple, &canonical, manifest.manifest_version)
                .expect("a missing title is upstream's problem, not a shim refusal");
        assert!(!request.body.contains_key("title"));

        // The plural spelling is not a gh flag, and must not slip through the
        // body plumbing as a single label.
        let plural = os_args(&["issue", "create", "--title", "Filing", "--labels=bug,p1"]);
        let Classification::Governed { tuple, canonical } = classify(&plural, &manifest, "macos")
        else {
            panic!("the verb is declared even when a flag is not");
        };
        let error = canonicalize_governed(&plural, &tuple, &canonical, manifest.manifest_version)
            .expect_err("--labels is not a gh flag");
        assert_eq!(error.code, RefusalCode::Unclassified);
    }

    #[test]
    fn v14_issue_create_refuses_flags_outside_bot_speech_before_routing() {
        let manifest = v14_manifest();
        assert_eq!(
            RefusalCode::UnsupportedFlag.as_str(),
            "gh_shim_unsupported_flag"
        );
        // Spelled out rather than read from CREATE_UNSUPPORTED_FLAGS: a test
        // that iterates the same list the classifier consults would still pass
        // if a flag were dropped from that list, which is the exact regression
        // it exists to catch.
        let refused = [
            "--assignee",
            "--milestone",
            "--project",
            "--web",
            "--template",
            "--recover",
        ];
        assert_eq!(
            CREATE_UNSUPPORTED_FLAGS, refused,
            "the refused set is part of the reviewed row, not an implementation detail"
        );
        for flag in refused {
            for spelling in [
                os_args(&["issue", "create", "--title", "Filing", flag, "someone"]),
                os_args(&[
                    "issue",
                    "create",
                    "--title",
                    "Filing",
                    &format!("{flag}=someone"),
                ]),
            ] {
                let Classification::Governed { tuple, canonical } =
                    classify(&spelling, &manifest, "macos")
                else {
                    panic!("{flag}: the verb is declared even when the flag is not");
                };
                let error =
                    canonicalize_governed(&spelling, &tuple, &canonical, manifest.manifest_version)
                        .expect_err(&format!("{flag} must refuse"));
                assert_eq!(
                    error.code,
                    RefusalCode::UnsupportedFlag,
                    "{flag} must refuse as an unsupported flag"
                );
                assert!(
                    error.text.starts_with(flag),
                    "{flag} refusal must name the flag: {}",
                    error.text
                );
                // The refusal happens while reading argv, so nothing is routed.
                assert_eq!(
                    refuse_governed_canonicalization(&error),
                    REFUSAL_EXIT_STATUS
                );
            }
        }
    }

    fn v16_manifest() -> Manifest {
        serde_json::from_str(include_str!("../tests/fixtures/gh_shim/v16-manifest.json"))
            .expect("synthetic v16 manifest fixture")
    }

    /// Classify under the v16 fixture and canonicalize, returning the request
    /// or the refusal the argv reader produced.
    fn canonicalize_pr_create(args: &[OsString]) -> Result<GovernedRequest, CanonicalizeError> {
        let manifest = v16_manifest();
        let Classification::Governed { tuple, canonical } = classify(args, &manifest, "macos")
        else {
            panic!("{args:?}: v16 pr create must be governed bot speech");
        };
        assert_eq!(tuple, "pr create");
        canonicalize_governed(args, &tuple, &canonical, manifest.manifest_version)
    }

    #[test]
    fn v16_pr_create_is_bot_speech_and_v15_leaves_it_undeclared() {
        let manifest = v16_manifest();
        manifest.validate().expect("valid v16 manifest");
        assert!(is_reviewed_governed_tuple(16, "pr create"));
        assert!(!is_reviewed_governed_tuple(15, "pr create"));
        // Merging is not speech and stays on the admin row.
        assert!(!is_reviewed_governed_tuple(16, "pr merge"));

        let args = os_args(&[
            "pr",
            "create",
            "-R",
            "cortexkit/aft",
            "--base",
            "main",
            "--head",
            "feature",
            "--title",
            "T",
        ]);
        assert!(matches!(
            classify(&args, &manifest, "macos"),
            Classification::Governed { ref tuple, .. } if tuple == "pr create"
        ));
        // The deployed v15 shape does not declare the verb at all.
        assert!(matches!(
            classify(&args, &v15_manifest(), "macos"),
            Classification::Unclassified
        ));
        // A manifest signed below 16 that nevertheless declares the row is
        // still refused: the version gate, not the row alone, admits it.
        let mut early = v16_manifest();
        early.manifest_version = 15;
        early.validate().expect("the row itself is well-formed");
        assert!(matches!(
            classify(&args, &early, "macos"),
            Classification::Unclassified
        ));
        assert_eq!(
            unclassified_refusal_text(&args, 15),
            "verb \"pr create\" is not declared in manifest 15 (output flags such as --json/-q are not the reason); GH_SHIM_BYPASS does not apply to undeclared invocations - this verb needs a manifest declaration"
        );
    }

    #[test]
    fn v16_pr_create_canonicalizes_to_the_declared_fields_only_request() {
        let manifest = v16_manifest();
        // The manifest's declaration, exactly: no target, five ordered fields.
        let declared = &manifest.canonicalization["pr create"];
        assert_eq!(declared.argv_forms, vec![FIELDS_ONLY_FORM.to_string()]);
        assert!(declared.target_fields.is_empty());
        assert_eq!(declared.body_fields, PR_CREATE_BODY_FIELDS);
        assert_eq!(
            PR_CREATE_BODY_FIELDS,
            ["title", "body", "base", "head", "draft"],
            "the field list is part of the signed row, not an implementation detail"
        );

        let directory = tempfile::tempdir().unwrap();
        let body_file = directory.path().join("f");
        fs::write(&body_file, "Line one\n\n- item\n").unwrap();
        let args = os_args(&[
            "pr",
            "create",
            "-R",
            "cortexkit/aft",
            "--base",
            "main",
            "--head",
            "feature",
            "--title",
            "T",
            "--body-file",
            body_file.to_str().unwrap(),
            "--draft",
        ]);
        let request = canonicalize_pr_create(&args).expect("pr create canonicalizes");
        assert_eq!(request.repository.as_deref(), Some("cortexkit/aft"));
        assert!(request.author_scope.is_none());
        assert!(GithubReadMutation::from_governed_request(&request).is_none());

        let determination = RungDetermination::r3(1, 16, &test_rung_provenance());
        let mut wire = governed_wire_request(&determination.record, "alfonso-aft", request);
        // The pid differs per test process; everything else is pinned.
        wire["metadata"]["pid"] = json!(0);
        // The body carries the declared fields in the manifest's order.
        assert_eq!(
            serde_json::to_string(&wire).unwrap(),
            concat!(
                r#"{"operation":"gh.route","gh_route_schema":1,"action":"pr create","target":{},"#,
                r#""body":{"title":"T","body":"Line one\n\n- item\n","base":"main","head":"feature","draft":true},"#,
                r#""repository":"cortexkit/aft","manifest_version":16,"rung_as_of_unix_secs":1,"#,
                r#""metadata":{"agent_id":"alfonso-aft","pid":0}}"#,
            )
        );

        // Without --draft the field is still sent, as false, so the holder
        // never has to guess a default. --body, the short spellings and the
        // attached forms reach the same fields.
        let short = os_args(&[
            "pr",
            "create",
            "--repo=cortexkit/aft",
            "-B",
            "main",
            "-H",
            "feature",
            "-t",
            "T",
            "-b",
            "B",
        ]);
        let request = canonicalize_pr_create(&short).expect("short spellings canonicalize");
        assert_eq!(
            Value::Object(request.body),
            json!({"title": "T", "body": "B", "base": "main", "head": "feature", "draft": false})
        );
        let attached = os_args(&[
            "pr",
            "create",
            "-R",
            "cortexkit/aft",
            "--base=main",
            "--head=feature",
            "--title=T",
            "-d",
        ]);
        let request = canonicalize_pr_create(&attached).expect("attached spellings canonicalize");
        assert_eq!(
            Value::Object(request.body),
            json!({"title": "T", "base": "main", "head": "feature", "draft": true})
        );
    }

    #[test]
    fn v16_pr_create_refuses_a_cross_repository_head_before_routing() {
        for head in ["someone:feature", "cortexkit:feature"] {
            let args = os_args(&[
                "pr",
                "create",
                "-R",
                "cortexkit/aft",
                "--base",
                "main",
                "--head",
                head,
                "--title",
                "T",
            ]);
            let error = canonicalize_pr_create(&args).expect_err("a fork head must refuse");
            assert_eq!(error.code, RefusalCode::UnsupportedFlag, "{head}");
            assert!(
                error.text.starts_with(&format!(
                    "--head {head}: cross-repository heads are refused"
                )),
                "{}",
                error.text
            );
            assert_eq!(
                refuse_governed_canonicalization(&error),
                REFUSAL_EXIT_STATUS
            );
        }
    }

    #[test]
    fn v16_pr_create_refuses_flags_the_governed_request_cannot_carry() {
        // Spelled out rather than read from PR_CREATE_UNSUPPORTED_FLAGS, so a
        // flag dropped from that list turns this test red.
        let refused = [
            "--assignee",
            "-a",
            "--reviewer",
            "-r",
            "--label",
            "-l",
            "--milestone",
            "-m",
            "--project",
            "-p",
            "--fill",
            "-f",
            "--fill-first",
            "--fill-verbose",
            "--web",
            "-w",
            "--editor",
            "-e",
            "--template",
            "-T",
            "--recover",
            "--dry-run",
            "--no-maintainer-edit",
        ];
        assert_eq!(PR_CREATE_UNSUPPORTED_FLAGS, refused);
        let base = [
            "pr",
            "create",
            "-R",
            "cortexkit/aft",
            "--base",
            "main",
            "--head",
            "feature",
            "--title",
            "T",
        ];
        for flag in refused {
            let attached = format!("{flag}=x");
            let mut spellings = vec![[&base[..], &[flag]].concat()];
            if flag.starts_with("--") {
                spellings.push([&base[..], &[attached.as_str()]].concat());
            }
            for spelling in spellings {
                let error = canonicalize_pr_create(&os_args(&spelling))
                    .expect_err(&format!("{flag} must refuse"));
                assert_eq!(error.code, RefusalCode::UnsupportedFlag, "{flag}");
                assert!(
                    error
                        .text
                        .starts_with(&format!("{flag}: pr create through the shim admits only")),
                    "{flag}: {}",
                    error.text
                );
            }
        }

        // Anything else unknown still refuses while argv is read, never
        // reaching upstream gh.
        for extra in [
            &["--draft=false"][..],
            &["--maintainer-can-modify"],
            &["42"],
        ] {
            let error = canonicalize_pr_create(&os_args(&[&base[..], extra].concat()))
                .expect_err("undeclared argv must refuse");
            assert_eq!(error.code, RefusalCode::Unclassified, "{extra:?}");
        }
        // A repeated field is refused rather than resolved to the last value.
        let error = canonicalize_pr_create(&os_args(&[&base[..], &["--title", "U"]].concat()))
            .expect_err("repeated --title must refuse");
        assert_eq!(error.text, "--title may be provided only once");
    }

    #[test]
    fn v16_pr_create_requires_base_head_and_title_instead_of_guessing() {
        let full = ["--base", "main", "--head", "feature", "--title", "T"];
        for (missing, name) in [(0, "--base"), (2, "--head"), (4, "--title")] {
            let mut args = vec!["pr", "create", "-R", "cortexkit/aft"];
            for (index, pair) in full.chunks(2).enumerate() {
                if index * 2 != missing {
                    args.extend_from_slice(pair);
                }
            }
            let error = canonicalize_pr_create(&os_args(&args))
                .expect_err(&format!("missing {name} must refuse"));
            assert_eq!(error.code, RefusalCode::Unclassified, "{name}");
            assert!(
                error
                    .text
                    .starts_with(&format!("pr create through the shim requires {name}:")),
                "{}",
                error.text
            );
        }
        // An empty value is the same as a missing one.
        let error = canonicalize_pr_create(&os_args(&[
            "pr",
            "create",
            "-R",
            "cortexkit/aft",
            "--base=",
            "--head",
            "feature",
            "--title",
            "T",
        ]))
        .expect_err("an empty --base must refuse");
        assert!(error.text.contains("requires --base"), "{}", error.text);
    }

    #[test]
    fn v16_pr_create_refuses_a_declaration_it_cannot_read() {
        let mut manifest = v16_manifest();
        manifest
            .canonicalization
            .get_mut("pr create")
            .unwrap()
            .body_fields
            .push("maintainer_can_modify".to_string());
        let args = os_args(&[
            "pr",
            "create",
            "-R",
            "cortexkit/aft",
            "--base",
            "main",
            "--head",
            "feature",
            "--title",
            "T",
        ]);
        let Classification::Governed { tuple, canonical } = classify(&args, &manifest, "macos")
        else {
            panic!("still declared");
        };
        let error = canonicalize_governed(&args, &tuple, &canonical, 16)
            .expect_err("a wider declaration must not be half-honoured");
        assert_eq!(error.code, RefusalCode::Unclassified);
    }

    #[test]
    fn v14_issue_edit_is_author_scoped_bot_speech_and_v13_leaves_it_undeclared() {
        let manifest = v14_manifest();
        manifest.validate().expect("valid v14 speech extensions");
        assert!(is_reviewed_governed_tuple(14, "issue edit"));
        assert!(!is_reviewed_governed_tuple(13, "issue edit"));

        let args = os_args(&["issue", "edit", "42", "--title", "T", "--body", "B"]);
        let Classification::Governed { tuple, canonical } = classify(&args, &manifest, "macos")
        else {
            panic!("v14 issue edit must be governed bot speech");
        };
        assert_eq!(tuple, "issue edit");
        let request = canonicalize_governed(&args, &tuple, &canonical, manifest.manifest_version)
            .expect("issue edit canonicalizes");
        assert_eq!(request.author_scope.as_deref(), Some("own"));
        let determination = RungDetermination::r3(1, 14, &test_rung_provenance());
        let wire = governed_wire_request(&determination.record, "agent-7", request);
        assert_eq!(wire["action"], "issue edit");
        assert_eq!(wire["target"]["number"], "42");
        assert_eq!(wire["body"]["title"], "T");
        assert_eq!(wire["body"]["body"], "B");
        assert_eq!(wire["author_scope"], "own");

        assert!(matches!(
            classify(&args, &v13_manifest(), "macos"),
            Classification::Unclassified
        ));
    }

    #[test]
    fn v14_issue_edit_collects_repeated_fields_and_reads_body_from_stdin() {
        let manifest = v14_manifest();
        let args = os_args(&[
            "issue",
            "edit",
            "--repo",
            "cortexkit/aft",
            "42",
            "--body-file",
            "-",
            "--add-label",
            "a",
            "--add-label=b",
            "--remove-label",
            "old",
            "--add-assignee=octocat",
            "--remove-assignee",
            "hubot",
        ]);
        let Classification::Governed { tuple, canonical } = classify(&args, &manifest, "macos")
        else {
            panic!("issue edit fields must remain on the governed route");
        };
        let mut stdin = std::io::Cursor::new("body supplied through stdin");
        let request = canonicalize_governed_from(
            &args,
            &tuple,
            &canonical,
            manifest.manifest_version,
            &mut stdin,
        )
        .expect("issue edit fields canonicalize");
        assert_eq!(request.body["body"], "body supplied through stdin");
        assert_eq!(request.body["add_labels"], json!(["a", "b"]));
        assert_eq!(request.body["remove_labels"], json!(["old"]));
        assert_eq!(request.body["add_assignees"], json!(["octocat"]));
        assert_eq!(request.body["remove_assignees"], json!(["hubot"]));
        assert_eq!(request.repository.as_deref(), Some("cortexkit/aft"));
    }

    /// The operator label row's argv for a manifest-declared `issue edit`.
    fn parse_label_row(args: &[&str]) -> Result<OperatorLabelEdit, CanonicalizeError> {
        let args = os_args(args);
        assert!(
            matches!(
                classify(&args, &v14_manifest(), "macos"),
                Classification::Governed { ref tuple, .. } if tuple == "issue edit"
            ),
            "{args:?} must reach the declared issue edit row"
        );
        parse_operator_label_edit(&args, LabelTarget::Issue)
    }

    #[test]
    fn v14_operator_label_row_accepts_every_label_form() {
        type Case<'a> = (
            &'a [&'a str],
            Option<&'a str>,
            u64,
            &'a [&'a str],
            &'a [&'a str],
        );
        let aft = Some("cortexkit/aft");
        let cases: &[Case] = &[
            (
                &["issue", "edit", "42", "--add-label", "bug"],
                None,
                42,
                &["bug"],
                &[],
            ),
            (
                &["issue", "edit", "42", "--add-label=bug,p1"],
                None,
                42,
                &["bug", "p1"],
                &[],
            ),
            (
                &["issue", "edit", "42", "--remove-label", "needs-triage"],
                None,
                42,
                &[],
                &["needs-triage"],
            ),
            (
                &["issue", "edit", "42", "--remove-label=a, b"],
                None,
                42,
                &[],
                &["a", "b"],
            ),
            (
                &[
                    "issue",
                    "edit",
                    "--add-label",
                    "x,y",
                    "7",
                    "--remove-label=z",
                ],
                None,
                7,
                &["x", "y"],
                &["z"],
            ),
            (
                &["issue", "edit", "42", "--add-label", "a", "--add-label=b"],
                None,
                42,
                &["a", "b"],
                &[],
            ),
            (
                &[
                    "issue",
                    "edit",
                    "42",
                    "--add-label",
                    "bug",
                    "--repo",
                    "CortexKit/AFT",
                ],
                aft,
                42,
                &["bug"],
                &[],
            ),
            (
                &[
                    "issue",
                    "edit",
                    "42",
                    "--add-label",
                    "bug",
                    "--repo=cortexkit/aft",
                ],
                aft,
                42,
                &["bug"],
                &[],
            ),
            (
                &[
                    "issue",
                    "edit",
                    "42",
                    "--add-label",
                    "bug",
                    "-R",
                    "cortexkit/aft",
                ],
                aft,
                42,
                &["bug"],
                &[],
            ),
            (
                &[
                    "issue",
                    "edit",
                    "42",
                    "--add-label",
                    "bug",
                    "-R=cortexkit/aft",
                ],
                aft,
                42,
                &["bug"],
                &[],
            ),
            (
                &[
                    "-R",
                    "cortexkit/aft",
                    "issue",
                    "edit",
                    "42",
                    "--add-label",
                    "bug",
                ],
                aft,
                42,
                &["bug"],
                &[],
            ),
            (
                &[
                    "--repo=cortexkit/aft",
                    "issue",
                    "edit",
                    "42",
                    "--remove-label",
                    "bug",
                ],
                aft,
                42,
                &[],
                &["bug"],
            ),
            (
                &[
                    "issue",
                    "edit",
                    "https://github.com/CortexKit/aft/issues/42",
                    "--add-label",
                    "bug",
                ],
                aft,
                42,
                &["bug"],
                &[],
            ),
            (
                &[
                    "issue",
                    "edit",
                    "https://github.com/cortexkit/aft/issues/42/",
                    "--add-label",
                    "bug",
                    "--repo",
                    "cortexkit/aft",
                ],
                aft,
                42,
                &["bug"],
                &[],
            ),
        ];
        for (argv, repository, issue_number, added, removed) in cases {
            let edit = parse_label_row(argv)
                .unwrap_or_else(|error| panic!("{argv:?} must be accepted: {}", error.text));
            assert_eq!(
                edit,
                OperatorLabelEdit {
                    repository: repository.map(str::to_string),
                    target: LabelTarget::Issue,
                    number: *issue_number,
                    labels_added: added.iter().map(|label| label.to_string()).collect(),
                    labels_removed: removed.iter().map(|label| label.to_string()).collect(),
                },
                "{argv:?}"
            );
        }
    }

    #[test]
    fn v14_operator_label_row_refuses_every_other_flag_by_name_even_beside_a_label() {
        // Spelled out rather than derived from any list the parser reads: the
        // row is "only labels", so every one of these must refuse whether or
        // not a valid label flag is also present.
        let forbidden = [
            "--body",
            "--body-file",
            "--title",
            "--milestone",
            "--remove-milestone",
            "--assignee",
            "--add-assignee",
            "--remove-assignee",
            "--project",
            "--add-project",
            "--remove-project",
            // Unknown to the row: issue create's label flag, a plural
            // misspelling, and a flag gh does not have at all.
            "--label",
            "--add-labels",
            "--frobnicate",
        ];
        for flag in forbidden {
            let inline = format!("{flag}=value");
            for argv in [
                vec!["issue", "edit", "42", flag, "value"],
                vec!["issue", "edit", "42", inline.as_str()],
                vec!["issue", "edit", "42", "--add-label", "bug", flag, "value"],
                vec!["issue", "edit", "42", flag, "value", "--remove-label=bug"],
                vec!["issue", "edit", "42", "--add-label=bug", inline.as_str()],
            ] {
                let error = parse_label_row(&argv)
                    .expect_err(&format!("{argv:?} must refuse: only labels are admitted"));
                assert_eq!(error.code, RefusalCode::UnsupportedFlag, "{argv:?}");
                assert!(
                    error.text.starts_with(&format!("{flag}: ")),
                    "{argv:?}: the refusal must name {flag}: {}",
                    error.text
                );
                assert!(
                    !error.text.contains("value"),
                    "{argv:?}: the refusal must not echo the flag's value: {}",
                    error.text
                );
            }
        }
        // A label flag cannot swallow a following flag as its value.
        let error = parse_label_row(&["issue", "edit", "42", "--add-label", "--body", "x"])
            .expect_err("a flag-shaped label value must refuse");
        assert!(error.text.starts_with("--add-label: "), "{}", error.text);
        let error = parse_label_row(&["issue", "edit", "42", "--remove-label="])
            .expect_err("an empty label value must refuse");
        assert!(error.text.starts_with("--remove-label: "), "{}", error.text);
        let error = parse_label_row(&["issue", "edit", "42", "--add-label", " , "])
            .expect_err("a value naming no label must refuse");
        assert!(error.text.starts_with("--add-label: "), "{}", error.text);
    }

    #[test]
    fn v14_operator_label_row_needs_exactly_one_issue_and_at_least_one_label_flag() {
        let error = parse_label_row(&["issue", "edit", "42", "43", "--add-label", "bug"])
            .expect_err("a second positional must refuse");
        assert!(error.text.starts_with("43: "), "{}", error.text);
        let error = parse_label_row(&[
            "issue",
            "edit",
            "42",
            "--add-label",
            "bug",
            "https://github.com/cortexkit/aft/issues/43",
        ])
        .expect_err("a second positional must refuse whatever its shape");
        assert!(
            error
                .text
                .starts_with("https://github.com/cortexkit/aft/issues/43: "),
            "{}",
            error.text
        );
        for (argv, named) in [
            (
                vec!["issue", "edit", "--add-label", "bug"],
                "no issue number or URL",
            ),
            (
                vec!["issue", "edit", "42"],
                "no --add-label or --remove-label",
            ),
            (
                vec!["issue", "edit", "42", "--repo", "cortexkit/aft"],
                "no --add-label or --remove-label",
            ),
            (vec!["issue", "edit", "#42", "--add-label", "bug"], "#42: "),
            (
                vec![
                    "issue",
                    "edit",
                    "https://github.com/o/r/pull/42",
                    "--add-label",
                    "bug",
                ],
                "https://github.com/o/r/pull/42: ",
            ),
            (
                vec![
                    "issue",
                    "edit",
                    "https://github.com/o/r/issues/42",
                    "--add-label",
                    "bug",
                    "-R",
                    "other/repo",
                ],
                "names o/r but --repo names other/repo",
            ),
            (
                vec![
                    "issue",
                    "edit",
                    "42",
                    "--add-label",
                    "bug",
                    "-R",
                    "a/b",
                    "--repo",
                    "a/b",
                ],
                "--repo: given more than once",
            ),
        ] {
            let error = parse_label_row(&argv).expect_err(&format!("{argv:?} must refuse"));
            assert!(error.text.contains(named), "{argv:?}: {}", error.text);
        }
    }

    #[test]
    fn v14_operator_label_row_audits_before_upstream_runs_and_v13_stays_unclassified() {
        use std::cell::Cell;

        let _env_lock = crate::test_env::process_env_lock();
        let _bypass = ScopedTestEnvVar::set("GH_SHIM_BYPASS", Some("operator"));
        let directory = tempfile::tempdir().expect("create label row state directory");
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let manifest = v14_manifest();
        assert!(is_reviewed_operator_label_tuple(14, "issue edit"));
        assert!(!is_reviewed_operator_label_tuple(13, "issue edit"));
        assert!(!is_reviewed_operator_label_tuple(14, "issue create"));
        let rung = RungDetermination::r3(TEST_NOW, 14, &test_rung_provenance()).record;
        let binding = AgentBinding {
            repo: "cortexkit/aft".to_string(),
            agent_id: "alfonso-aft".to_string(),
        };
        let args = os_args(&[
            "issue",
            "edit",
            "42",
            "--add-label",
            "triaged,p1",
            "--remove-label=needs-triage",
            "--repo",
            "CortexKit/AFT",
        ]);

        let delegated = Cell::new(0);
        let status = dispatch_r3(
            &args,
            classify(&args, &manifest, "macos"),
            &manifest,
            &paths,
            &rung,
            &binding,
            TEST_NOW,
            |delegated_args| {
                // Stand-in for upstream gh: the audit line must already be on
                // disk when it starts, so a crash mid-call is still recorded.
                assert!(
                    paths.bypass_audit.exists(),
                    "the bypass audit must exist before upstream gh runs"
                );
                let (records, error) = read_bypass_audit(&paths);
                assert!(error.is_none(), "{error:?}");
                assert_eq!(
                    records.expect("audit records"),
                    vec![json!({
                        "as_of_unix_secs": TEST_NOW,
                        "tuple": "issue edit",
                        "repository": "cortexkit/aft",
                        "issue_number": 42,
                        "labels_added": ["triaged", "p1"],
                        "labels_removed": ["needs-triage"],
                    })]
                );
                assert_eq!(delegated_args, args, "upstream gh gets the argv unchanged");
                delegated.set(delegated.get() + 1);
                73
            },
        );
        assert_eq!(status, 73);
        assert_eq!(delegated.get(), 1);

        // A label edit carrying a body refuses before the audit or upstream.
        let with_body = os_args(&["issue", "edit", "42", "--add-label", "bug", "--body", "x"]);
        let status = dispatch_r3(
            &with_body,
            classify(&with_body, &manifest, "macos"),
            &manifest,
            &paths,
            &rung,
            &binding,
            TEST_NOW,
            |_| panic!("a non-label issue edit reached upstream gh under the bypass"),
        );
        assert_eq!(status, REFUSAL_EXIT_STATUS);
        assert_eq!(read_bypass_audit(&paths).0.expect("audit").len(), 1);

        // Under the deployed v13 manifest the row does not exist: the same
        // argv stays unclassified, and the bypass does not reach it.
        let v13 = v13_manifest();
        assert_eq!(v13.manifest_version, 13);
        let classification = classify(&args, &v13, "macos");
        assert!(matches!(classification, Classification::Unclassified));
        let status = dispatch_r3(
            &args,
            classification,
            &v13,
            &paths,
            &rung,
            &binding,
            TEST_NOW,
            |_| panic!("v13 issue edit reached upstream gh under the bypass"),
        );
        assert_eq!(status, REFUSAL_EXIT_STATUS);
        assert_eq!(read_bypass_audit(&paths).0.expect("audit").len(), 1);
    }

    /// The flag sections of `gh pr edit --help` and `gh label create --help`,
    /// captured verbatim from gh 2.97.0. The row tests read the flag set from
    /// this text rather than from memory, so a flag gh lists is either
    /// admitted by its row or refused by name.
    const GH_PR_EDIT_HELP_FLAGS: &str = "\
FLAGS
      --add-assignee login      Add assigned users by their login. Use \"@me\" to assign yourself, or \"@copilot\" to assign Copilot.
      --add-label name          Add labels by name
      --add-project title       Add the pull request to projects by title
      --add-reviewer login      Add or re-request reviewers by their login. Use \"@copilot\" to request review from Copilot.
  -B, --base branch             Change the base branch for this pull request
  -b, --body string             Set the new body.
  -F, --body-file file          Read body text from file (use \"-\" to read from standard input)
  -m, --milestone name          Edit the milestone the pull request belongs to by name
      --remove-assignee login   Remove assigned users by their login. Use \"@me\" to unassign yourself, or \"@copilot\" to unassign Copilot.
      --remove-label name       Remove labels by name
      --remove-milestone        Remove the milestone association from the pull request
      --remove-project title    Remove the pull request from projects by title
      --remove-reviewer login   Remove reviewers by their login. Use \"@copilot\" to remove review request from Copilot.
  -t, --title string            Set the new title.

INHERITED FLAGS
      --help                     Show help for command
  -R, --repo [HOST/]OWNER/REPO   Select another repository using the [HOST/]OWNER/REPO format
";
    const GH_LABEL_CREATE_HELP_FLAGS: &str = "\
FLAGS
  -c, --color string         Color of the label
  -d, --description string   Description of the label
  -f, --force                Update the label color and description if label already exists

INHERITED FLAGS
      --help                     Show help for command
  -R, --repo [HOST/]OWNER/REPO   Select another repository using the [HOST/]OWNER/REPO format
";

    /// One flag row of a captured help text: its spellings (short first when
    /// present) and whether it takes a value.
    fn help_flags(help: &str) -> Vec<(Vec<String>, bool)> {
        help.lines()
            .filter(|line| line.trim_start().starts_with('-'))
            .map(|line| {
                // The first column ends at the first run of two spaces.
                let column = line.trim_start().split("  ").next().unwrap_or_default();
                let words = column.split_whitespace().collect::<Vec<_>>();
                let spellings = words
                    .iter()
                    .filter(|word| word.starts_with('-'))
                    .map(|word| word.trim_end_matches(',').to_string())
                    .collect::<Vec<_>>();
                let takes_value = words.last().is_some_and(|word| !word.starts_with('-'));
                (spellings, takes_value)
            })
            .collect()
    }

    #[test]
    fn captured_help_texts_parse_into_the_expected_flag_sets() {
        let label_create = help_flags(GH_LABEL_CREATE_HELP_FLAGS);
        assert_eq!(
            label_create,
            vec![
                (vec!["-c".to_string(), "--color".to_string()], true),
                (vec!["-d".to_string(), "--description".to_string()], true),
                (vec!["-f".to_string(), "--force".to_string()], false),
                (vec!["--help".to_string()], false),
                (vec!["-R".to_string(), "--repo".to_string()], true),
            ]
        );
        let pr_edit = help_flags(GH_PR_EDIT_HELP_FLAGS);
        assert_eq!(pr_edit.len(), 16);
        assert!(pr_edit.contains(&(vec!["--remove-milestone".to_string()], false)));
        assert!(pr_edit.contains(&(vec!["-B".to_string(), "--base".to_string()], true)));
    }

    /// The operator label row's argv for a manifest-declared `pr edit`.
    fn parse_pr_label_row(args: &[&str]) -> Result<OperatorLabelEdit, CanonicalizeError> {
        let args = os_args(args);
        assert!(
            matches!(
                classify(&args, &v14_manifest(), "macos"),
                Classification::Admin { ref tuple } if tuple == "pr edit"
            ),
            "{args:?} must reach the declared pr edit row"
        );
        parse_operator_label_edit(&args, LabelTarget::PullRequest)
    }

    #[test]
    fn v14_operator_pr_label_row_accepts_every_label_form() {
        type Case<'a> = (
            &'a [&'a str],
            Option<&'a str>,
            u64,
            &'a [&'a str],
            &'a [&'a str],
        );
        let aft = Some("cortexkit/aft");
        let cases: &[Case] = &[
            (
                &["pr", "edit", "9", "--add-label", "trivial"],
                None,
                9,
                &["trivial"],
                &[],
            ),
            (
                &["pr", "edit", "9", "--add-label=a,b"],
                None,
                9,
                &["a", "b"],
                &[],
            ),
            (
                &["pr", "edit", "9", "--remove-label", "wip"],
                None,
                9,
                &[],
                &["wip"],
            ),
            (
                &["pr", "edit", "--add-label", "x", "9", "--remove-label=y"],
                None,
                9,
                &["x"],
                &["y"],
            ),
            (
                &[
                    "pr",
                    "edit",
                    "9",
                    "--add-label",
                    "trivial",
                    "-R",
                    "CortexKit/AFT",
                ],
                aft,
                9,
                &["trivial"],
                &[],
            ),
            (
                &[
                    "--repo=cortexkit/aft",
                    "pr",
                    "edit",
                    "9",
                    "--add-label",
                    "trivial",
                ],
                aft,
                9,
                &["trivial"],
                &[],
            ),
            (
                &[
                    "pr",
                    "edit",
                    "https://github.com/CortexKit/aft/pull/9/",
                    "--add-label",
                    "trivial",
                    "--repo",
                    "cortexkit/aft",
                ],
                aft,
                9,
                &["trivial"],
                &[],
            ),
        ];
        for (argv, repository, number, added, removed) in cases {
            let edit = parse_pr_label_row(argv)
                .unwrap_or_else(|error| panic!("{argv:?} must be accepted: {}", error.text));
            assert_eq!(
                edit,
                OperatorLabelEdit {
                    repository: repository.map(str::to_string),
                    target: LabelTarget::PullRequest,
                    number: *number,
                    labels_added: added.iter().map(|label| label.to_string()).collect(),
                    labels_removed: removed.iter().map(|label| label.to_string()).collect(),
                },
                "{argv:?}"
            );
        }
    }

    #[test]
    fn v14_operator_pr_label_row_refuses_every_other_flag_by_name_even_beside_a_label() {
        // Every flag `gh pr edit --help` lists except the two label flags and
        // --repo, plus flags gh does not have for this verb at all.
        let admitted = ["--add-label", "--remove-label", "--repo", "-R"];
        let mut forbidden = help_flags(GH_PR_EDIT_HELP_FLAGS)
            .into_iter()
            .flat_map(|(spellings, _)| spellings)
            .filter(|flag| !admitted.contains(&flag.as_str()))
            .collect::<Vec<_>>();
        forbidden.extend(["--label", "--add-labels", "--frobnicate"].map(str::to_string));
        assert!(forbidden.len() >= 20, "{forbidden:?}");
        for flag in &forbidden {
            let flag = flag.as_str();
            let inline = format!("{flag}=value");
            let mut spellings = vec![
                vec!["pr", "edit", "9", flag, "value"],
                vec!["pr", "edit", "9", inline.as_str()],
                vec!["pr", "edit", "9", "--add-label", "trivial", flag, "value"],
                vec!["pr", "edit", "9", flag, "value", "--remove-label=trivial"],
                vec!["pr", "edit", "9", "--add-label=trivial", inline.as_str()],
            ];
            // pflag also takes a short flag's value attached: `-bvalue`.
            let attached = format!("{flag}value");
            if !flag.starts_with("--") {
                spellings.push(vec![
                    "pr",
                    "edit",
                    "9",
                    "--add-label=trivial",
                    attached.as_str(),
                ]);
            }
            for argv in spellings {
                let error = parse_pr_label_row(&argv)
                    .expect_err(&format!("{argv:?} must refuse: only labels are admitted"));
                assert_eq!(error.code, RefusalCode::UnsupportedFlag, "{argv:?}");
                assert!(
                    error.text.starts_with(&format!("{flag}: ")),
                    "{argv:?}: the refusal must name {flag}: {}",
                    error.text
                );
                assert!(
                    !error.text.contains("value"),
                    "{argv:?}: the refusal must not echo the flag's value: {}",
                    error.text
                );
            }
        }
    }

    #[test]
    fn v14_operator_pr_label_row_needs_exactly_one_pr_and_at_least_one_label_flag() {
        let error = parse_pr_label_row(&["pr", "edit", "9", "10", "--add-label", "trivial"])
            .expect_err("a second positional must refuse");
        assert!(error.text.starts_with("10: "), "{}", error.text);
        for (argv, named) in [
            (
                vec!["pr", "edit", "--add-label", "trivial"],
                "no pull request number or URL",
            ),
            (vec!["pr", "edit", "9"], "no --add-label or --remove-label"),
            // gh would resolve a branch name to a pull request; the audit line
            // must record a number, so the row refuses it.
            (
                vec!["pr", "edit", "feature/x", "--add-label", "trivial"],
                "feature/x: ",
            ),
            (
                vec![
                    "pr",
                    "edit",
                    "https://github.com/o/r/issues/9",
                    "--add-label",
                    "trivial",
                ],
                "https://github.com/o/r/issues/9: ",
            ),
            (
                vec![
                    "pr",
                    "edit",
                    "https://github.com/o/r/pull/9",
                    "--add-label",
                    "trivial",
                    "-R",
                    "other/repo",
                ],
                "names o/r but --repo names other/repo",
            ),
        ] {
            let error = parse_pr_label_row(&argv).expect_err(&format!("{argv:?} must refuse"));
            assert!(error.text.contains(named), "{argv:?}: {}", error.text);
        }
    }

    #[test]
    fn v14_operator_pr_label_row_audits_before_upstream_runs_and_v13_stays_unclassified() {
        use std::cell::Cell;

        let _env_lock = crate::test_env::process_env_lock();
        let directory = tempfile::tempdir().expect("create label row state directory");
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let manifest = v14_manifest();
        manifest
            .validate()
            .expect("valid v14 manifest with operator rows");
        assert!(is_reviewed_operator_row_admin_tuple(14, "pr edit"));
        assert!(!is_reviewed_operator_row_admin_tuple(13, "pr edit"));
        let rung = RungDetermination::r3(TEST_NOW, 14, &test_rung_provenance()).record;
        let binding = AgentBinding {
            repo: "cortexkit/aft".to_string(),
            agent_id: "alfonso-aft".to_string(),
        };
        let args = os_args(&[
            "pr",
            "edit",
            "9",
            "--add-label",
            "trivial",
            "--remove-label=needs-design",
            "-R",
            "CortexKit/AFT",
        ]);
        let dispatch = |args: &[OsString], manifest: &Manifest, upstream: &dyn Fn() -> i32| {
            dispatch_r3(
                args,
                classify(args, manifest, "macos"),
                manifest,
                &paths,
                &rung,
                &binding,
                TEST_NOW,
                |_| upstream(),
            )
        };

        // Without the bypass `pr edit` refuses exactly as an undeclared verb
        // did before v14: no audit, no upstream.
        {
            let _bypass = ScopedTestEnvVar::set("GH_SHIM_BYPASS", None);
            let status = dispatch(&args, &manifest, &|| {
                panic!("pr edit reached upstream gh without the bypass")
            });
            assert_eq!(status, REFUSAL_EXIT_STATUS);
            assert!(!paths.bypass_audit.exists());
        }

        let _bypass = ScopedTestEnvVar::set("GH_SHIM_BYPASS", Some("operator"));
        let delegated = Cell::new(0);
        let status = dispatch(&args, &manifest, &|| {
            // Stand-in for upstream gh: the audit line must already be on disk.
            let (records, error) = read_bypass_audit(&paths);
            assert!(error.is_none(), "{error:?}");
            assert_eq!(
                records.expect("audit records before upstream gh runs"),
                vec![json!({
                    "as_of_unix_secs": TEST_NOW,
                    "tuple": "pr edit",
                    "repository": "cortexkit/aft",
                    "pr_number": 9,
                    "labels_added": ["trivial"],
                    "labels_removed": ["needs-design"],
                })]
            );
            delegated.set(delegated.get() + 1);
            73
        });
        assert_eq!(status, 73);
        assert_eq!(delegated.get(), 1);

        // A label edit carrying a title refuses before the audit or upstream.
        let with_title = os_args(&["pr", "edit", "9", "--add-label", "trivial", "-t", "x"]);
        let status = dispatch(&with_title, &manifest, &|| {
            panic!("a non-label pr edit reached upstream gh under the bypass")
        });
        assert_eq!(status, REFUSAL_EXIT_STATUS);
        assert_eq!(read_bypass_audit(&paths).0.expect("audit").len(), 1);

        // Under the deployed v13 manifest the row does not exist.
        let v13 = v13_manifest();
        assert!(matches!(
            classify(&args, &v13, "macos"),
            Classification::Unclassified
        ));
        let status = dispatch(&args, &v13, &|| {
            panic!("v13 pr edit reached upstream gh under the bypass")
        });
        assert_eq!(status, REFUSAL_EXIT_STATUS);
        assert_eq!(read_bypass_audit(&paths).0.expect("audit").len(), 1);
    }

    fn parse_label_create_row(args: &[&str]) -> Result<OperatorLabelCreate, CanonicalizeError> {
        let args = os_args(args);
        assert!(
            matches!(
                classify(&args, &v14_manifest(), "macos"),
                Classification::Admin { ref tuple } if tuple == "label create"
            ),
            "{args:?} must reach the declared label create row"
        );
        parse_operator_label_create(&args)
    }

    #[test]
    fn v14_operator_label_create_row_admits_every_flag_gh_lists() {
        // Each flag from the captured help, in every spelling, beside a name.
        for (spellings, takes_value) in help_flags(GH_LABEL_CREATE_HELP_FLAGS) {
            // --help only prints usage; it is not part of creating a label.
            if spellings == ["--help"] {
                continue;
            }
            for spelling in &spellings {
                let spelling = spelling.as_str();
                let inline = format!("{spelling}=0E8A16");
                let argvs = if takes_value {
                    vec![
                        vec!["label", "create", "design-approved", spelling, "0E8A16"],
                        vec!["label", "create", "design-approved", inline.as_str()],
                    ]
                } else {
                    vec![vec!["label", "create", "design-approved", spelling]]
                };
                for argv in argvs {
                    parse_label_create_row(&argv).unwrap_or_else(|error| {
                        panic!("{argv:?} must be accepted: {}", error.text)
                    });
                }
            }
        }

        let create = parse_label_create_row(&[
            "-R",
            "CortexKit/AFT",
            "label",
            "create",
            "design-approved",
            "--color",
            "0E8A16",
            "-d",
            "Design gate passed",
            "-f",
        ])
        .expect("full label create");
        assert_eq!(
            create,
            OperatorLabelCreate {
                repository: Some("cortexkit/aft".to_string()),
                label: "design-approved".to_string(),
                color: Some("0E8A16".to_string()),
            }
        );
        let create = parse_label_create_row(&["label", "create", "trivial"]).expect("name only");
        assert_eq!(create.color, None);
        assert_eq!(create.repository, None);
    }

    #[test]
    fn v14_operator_label_create_row_refuses_every_other_flag_and_a_second_name() {
        // Flags from sibling label verbs, output flags, and a flag gh does
        // not have: none is part of creating one label.
        for flag in [
            "--name",
            "--new-name",
            "--yes",
            "--confirm",
            "--json",
            "--web",
            "--label",
            "--frobnicate",
            "-x",
        ] {
            let inline = format!("{flag}=value");
            for argv in [
                vec!["label", "create", "bug", flag, "value"],
                vec!["label", "create", "bug", inline.as_str()],
                vec!["label", "create", "bug", "--color", "E99695", flag, "value"],
                vec!["label", "create", "bug", "-f", inline.as_str()],
            ] {
                let error =
                    parse_label_create_row(&argv).expect_err(&format!("{argv:?} must refuse"));
                assert_eq!(error.code, RefusalCode::UnsupportedFlag, "{argv:?}");
                assert!(
                    error.text.starts_with(&format!("{flag}: ")),
                    "{argv:?}: the refusal must name {flag}: {}",
                    error.text
                );
                assert!(!error.text.contains("value"), "{argv:?}: {}", error.text);
            }
        }
        for (argv, named) in [
            (
                vec!["label", "create", "bug", "urgent"],
                "urgent: a second positional",
            ),
            (
                vec!["label", "create", "bug", "-c", "E99695", "urgent"],
                "urgent: ",
            ),
            (
                vec!["label", "create", "--color", "E99695"],
                "no label name",
            ),
            (
                vec!["label", "create", "bug", "--color"],
                "--color: requires a value",
            ),
            (
                vec!["label", "create", "bug", "-c", "E99695", "--color=000000"],
                "--color: given more than once",
            ),
            (
                vec!["label", "create", "bug", "-R", "a/b", "--repo", "a/b"],
                "--repo: given more than once",
            ),
        ] {
            let error = parse_label_create_row(&argv).expect_err(&format!("{argv:?} must refuse"));
            assert!(error.text.contains(named), "{argv:?}: {}", error.text);
        }
    }

    #[test]
    fn v14_operator_label_create_row_audits_before_upstream_runs_and_v13_stays_unclassified() {
        use std::cell::Cell;

        let _env_lock = crate::test_env::process_env_lock();
        let directory = tempfile::tempdir().expect("create label row state directory");
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let manifest = v14_manifest();
        assert!(is_reviewed_operator_row_admin_tuple(14, "label create"));
        assert!(!is_reviewed_operator_row_admin_tuple(13, "label create"));
        assert!(!is_reviewed_operator_row_admin_tuple(14, "label delete"));
        assert!(!is_reviewed_operator_row_admin_tuple(14, "label edit"));
        let rung = RungDetermination::r3(TEST_NOW, 14, &test_rung_provenance()).record;
        let binding = AgentBinding {
            repo: "cortexkit/aft".to_string(),
            agent_id: "alfonso-aft".to_string(),
        };
        let args = os_args(&[
            "label",
            "create",
            "design-approved",
            "--color=0E8A16",
            "--description",
            "Design gate passed",
            "--repo",
            "CortexKit/AFT",
        ]);
        let dispatch = |args: &[OsString], manifest: &Manifest, upstream: &dyn Fn() -> i32| {
            dispatch_r3(
                args,
                classify(args, manifest, "macos"),
                manifest,
                &paths,
                &rung,
                &binding,
                TEST_NOW,
                |_| upstream(),
            )
        };

        {
            let _bypass = ScopedTestEnvVar::set("GH_SHIM_BYPASS", None);
            let status = dispatch(&args, &manifest, &|| {
                panic!("label create reached upstream gh without the bypass")
            });
            assert_eq!(status, REFUSAL_EXIT_STATUS);
            assert!(!paths.bypass_audit.exists());
        }

        let _bypass = ScopedTestEnvVar::set("GH_SHIM_BYPASS", Some("operator"));
        let delegated = Cell::new(0);
        let status = dispatch(&args, &manifest, &|| {
            // Stand-in for upstream gh: the audit line must already be on disk.
            let (records, error) = read_bypass_audit(&paths);
            assert!(error.is_none(), "{error:?}");
            assert_eq!(
                records.expect("audit records before upstream gh runs"),
                vec![json!({
                    "as_of_unix_secs": TEST_NOW,
                    "tuple": "label create",
                    "repository": "cortexkit/aft",
                    "label": "design-approved",
                    "color": "0E8A16",
                })]
            );
            delegated.set(delegated.get() + 1);
            73
        });
        assert_eq!(status, 73);
        assert_eq!(delegated.get(), 1);

        // An extra flag refuses before the audit or upstream.
        let extra = os_args(&["label", "create", "bug", "--frobnicate"]);
        let status = dispatch(&extra, &manifest, &|| {
            panic!("a label create outside the row reached upstream gh")
        });
        assert_eq!(status, REFUSAL_EXIT_STATUS);
        assert_eq!(read_bypass_audit(&paths).0.expect("audit").len(), 1);

        // Deletion and edits stay undeclared even under the bypass.
        for other in [
            os_args(&["label", "delete", "bug", "--yes"]),
            os_args(&["label", "edit", "bug", "--color", "000000"]),
        ] {
            assert!(matches!(
                classify(&other, &manifest, "macos"),
                Classification::Unclassified
            ));
            let status = dispatch(&other, &manifest, &|| {
                panic!("{other:?} reached upstream gh under the bypass")
            });
            assert_eq!(status, REFUSAL_EXIT_STATUS);
        }

        // Under the deployed v13 manifest the row does not exist.
        let v13 = v13_manifest();
        assert!(matches!(
            classify(&args, &v13, "macos"),
            Classification::Unclassified
        ));
        let status = dispatch(&args, &v13, &|| {
            panic!("v13 label create reached upstream gh under the bypass")
        });
        assert_eq!(status, REFUSAL_EXIT_STATUS);
        assert_eq!(read_bypass_audit(&paths).0.expect("audit").len(), 1);
    }

    #[test]
    fn v14_issue_edit_refuses_planning_flags_and_leaves_delete_undeclared() {
        let manifest = v14_manifest();
        for flag in [
            "--milestone",
            "--remove-milestone",
            "--project",
            "--add-project",
            "--remove-project",
        ] {
            for spelling in [
                os_args(&["issue", "edit", "42", flag, "roadmap"]),
                os_args(&["issue", "edit", "42", &format!("{flag}=roadmap")]),
            ] {
                let Classification::Governed { tuple, canonical } =
                    classify(&spelling, &manifest, "macos")
                else {
                    panic!("{flag}: the verb is declared even when the flag is not");
                };
                let error =
                    canonicalize_governed(&spelling, &tuple, &canonical, manifest.manifest_version)
                        .expect_err(&format!("{flag} must refuse"));
                assert_eq!(error.code, RefusalCode::DestructiveFlag);
                assert!(
                    error.text.starts_with(flag),
                    "{flag} refusal must name the flag: {}",
                    error.text
                );
            }
        }

        let delete = os_args(&["issue", "delete", "42"]);
        assert!(matches!(
            classify(&delete, &manifest, "macos"),
            Classification::Unclassified
        ));
    }

    #[test]
    fn v14_own_comment_patch_is_governed_and_v13_leaves_it_undeclared() {
        let manifest = v14_manifest();
        let args = os_args(&[
            "api",
            "--method",
            "PATCH",
            "repos/cortexkit/aft/issues/comments/123",
            "--input",
            "-",
        ]);
        let Classification::Governed { tuple, canonical } = classify(&args, &manifest, "macos")
        else {
            panic!("v14 own-comment PATCH must be governed bot speech");
        };
        assert_eq!(tuple, "api:PATCH:/repos/*/*/issues/comments/*");

        let mut stdin = std::io::Cursor::new(r#"{"body":"edited prose"}"#);
        let request = canonicalize_governed_api_from(
            &args,
            &tuple,
            &canonical,
            manifest.manifest_version,
            &mut stdin,
        )
        .expect("own-comment PATCH canonicalizes");
        assert_eq!(request.target["comment_id"], json!("123"));
        assert_eq!(request.body["body"], json!("edited prose"));
        assert_eq!(request.repository.as_deref(), Some("cortexkit/aft"));
        assert_eq!(request.author_scope.as_deref(), Some("own"));
        let determination = RungDetermination::r3(1, 14, &test_rung_provenance());
        let wire = governed_wire_request(&determination.record, "agent-7", request);
        assert_eq!(wire["author_scope"], "own");

        // The deployed v13 shape declares no PATCH rule at all, so the same
        // invocation is undeclared there.
        assert!(matches!(
            classify(&args, &v13_manifest(), "macos"),
            Classification::Unclassified
        ));
    }

    #[test]
    fn v14_governed_patch_row_admits_only_the_comment_endpoint_and_a_body_payload() {
        let manifest = v14_manifest();
        // The issue itself is a different resource: editing it is not an
        // own-comment edit, and the new row must not reach it.
        for endpoint in [
            "repos/cortexkit/aft/issues/123",
            "repos/cortexkit/aft/issues/comments",
        ] {
            let args = os_args(&["api", "--method", "PATCH", endpoint, "--input", "-"]);
            assert!(
                matches!(
                    classify(&args, &manifest, "macos"),
                    Classification::Unclassified
                ),
                "{endpoint} must not be admitted by the own-comment row"
            );
        }

        let args = os_args(&[
            "api",
            "--method",
            "PATCH",
            "repos/cortexkit/aft/issues/comments/123",
            "--input",
            "-",
        ]);
        let Classification::Governed { tuple, canonical } = classify(&args, &manifest, "macos")
        else {
            panic!("the declared endpoint must be governed");
        };
        // A payload carrying more than the body is not the declared request.
        let mut stdin = std::io::Cursor::new(r#"{"body":"edited","state":"closed"}"#);
        let error = canonicalize_governed_api_from(
            &args,
            &tuple,
            &canonical,
            manifest.manifest_version,
            &mut stdin,
        )
        .expect_err("a body-only row must refuse a wider payload");
        assert_eq!(error.code, RefusalCode::Unclassified);

        // An output flag would be silently dropped, because a governed route
        // never runs upstream gh.
        let with_jq = os_args(&[
            "api",
            "--method",
            "PATCH",
            "repos/cortexkit/aft/issues/comments/123",
            "-f",
            "body=edited",
            "--jq",
            ".id",
        ]);
        let mut empty = std::io::Cursor::new("");
        let error = canonicalize_governed_api_from(
            &with_jq,
            &tuple,
            &canonical,
            manifest.manifest_version,
            &mut empty,
        )
        .expect_err("an undeclared flag must refuse");
        assert_eq!(error.code, RefusalCode::Unclassified);

        // The field spelling of the same payload is the declared request.
        let field_args = os_args(&[
            "api",
            "--method",
            "PATCH",
            "repos/cortexkit/aft/issues/comments/123",
            "-f",
            "body=edited",
        ]);
        let request = canonicalize_governed_api_from(
            &field_args,
            &tuple,
            &canonical,
            manifest.manifest_version,
            &mut empty,
        )
        .expect("a body field is the declared payload");
        assert_eq!(request.body["body"], json!("edited"));
    }

    #[test]
    fn v14_manifest_payload_round_trips_through_serialization() {
        let bytes = serde_json::to_vec(&v14_manifest()).expect("serialize the v14 shape");
        let parsed: Manifest = serde_json::from_slice(&bytes).expect("parse the v14 shape back");
        parsed.validate().expect("the parsed v14 shape is valid");
        assert_eq!(parsed.manifest_version, 14);

        let create = os_args(&["issue", "create", "--title", "Filing"]);
        assert!(matches!(
            classify(&create, &parsed, "macos"),
            Classification::Governed { ref tuple, .. } if tuple == "issue create"
        ));
        let edit = os_args(&["issue", "edit", "42", "--title", "Retitled"]);
        assert!(matches!(
            classify(&edit, &parsed, "macos"),
            Classification::Governed { ref tuple, .. } if tuple == "issue edit"
        ));
        let patch = os_args(&[
            "api",
            "--method",
            "PATCH",
            "/repos/cortexkit/aft/issues/comments/123",
            "--input",
            "-",
        ]);
        assert!(matches!(
            classify(&patch, &parsed, "macos"),
            Classification::Governed { ref tuple, .. }
                if tuple == "api:PATCH:/repos/*/*/issues/comments/*"
        ));
    }

    #[test]
    fn admin_tier_refusal_names_the_verb_and_the_sanctioned_operator_path() {
        // The bypass is the audited way to run an administration-tier verb, so
        // the text describes whose identity the call runs under instead of
        // reading as a prohibition or a bare instruction to set a variable.
        assert_eq!(
            admin_refusal_text("pr merge"),
            "`pr merge` is administration-tier — it runs under the operator's identity, not the bot's. Re-run with GH_SHIM_BYPASS=operator; the shim records an operator-attributed audit line."
        );
        assert_eq!(RefusalCode::AdminTier.as_str(), "gh_shim_admin_tier");
        assert_eq!(
            refuse(RefusalCode::AdminTier, &admin_refusal_text("pr merge")),
            REFUSAL_EXIT_STATUS
        );
    }

    #[test]
    fn unclassified_refusal_names_the_verb_the_classifier_decided_on() {
        // Two-word verb: the decision is about `issue create`, not the tail.
        assert_eq!(
            unclassified_refusal_text(
                &os_args(&["issue", "create", "--title", "Filing", "--json", "number"]),
                13
            ),
            "verb \"issue create\" is not declared in manifest 13 (output flags such as --json/-q are not the reason); GH_SHIM_BYPASS does not apply to undeclared invocations - this verb needs a manifest declaration"
        );
        // Raw API calls name the effective method and normalized endpoint.
        assert_eq!(
            unclassified_refusal_text(
                &os_args(&[
                    "api",
                    "--method",
                    "DELETE",
                    "repos/cortexkit/aft/actions/runs/123"
                ]),
                9
            ),
            "api DELETE /repos/cortexkit/aft/actions/runs/123 is not declared in manifest 9 (output flags such as --json/-q are not the reason); GH_SHIM_BYPASS does not apply to undeclared invocations - this verb needs a manifest declaration"
        );
    }

    #[test]
    fn unclassified_api_refusal_names_graphql_mutations_and_uninspectable_queries() {
        for (tail, subject) in [
            (
                &["-f", "query=mutation { addStar }"][..],
                "api graphql (mutation)",
            ),
            (
                &[
                    "--field=query=query Read { viewer { login } } mutation Write { addStar }",
                    "-f",
                    "operationName=Write",
                ],
                "api graphql (mutation)",
            ),
            (&["-F", "query=@-"], "api graphql (uninspectable query)"),
            (
                &["--input", "body.json"],
                "api graphql (uninspectable query)",
            ),
            (
                &["-f", "query=subscription { events }"],
                "api graphql (uninspectable query)",
            ),
        ] {
            let mut args = os_args(&["api", "graphql"]);
            args.extend(os_args(tail));
            let text = unclassified_refusal_text(&args, 17);
            assert!(
                text.starts_with(&format!("{subject} is not declared in manifest 17")),
                "{text}"
            );
        }
    }

    #[test]
    fn admin_api_rule_classifies_field_bearing_branch_protection_puts() {
        let input_args = os_args(&[
            "api",
            "-X",
            "PUT",
            "/repos/o/r/branches/main/protection",
            "--input",
            "body.json",
        ]);
        let admin_manifest = branch_protection_manifest("PUT", Tier::Admin);
        assert!(matches!(
            classify(&input_args, &admin_manifest, "macos"),
            Classification::Admin { ref tuple } if tuple == BRANCH_PROTECTION_API_TUPLE
        ));
        assert!(matches!(
            classify(&input_args, &v12_fixture_manifest(), "macos"),
            Classification::Unclassified
        ));
        assert!(matches!(
            classify(
                &input_args,
                &branch_protection_manifest("PUT", Tier::Governed),
                "macos"
            ),
            Classification::Unclassified
        ));

        let field_args = os_args(&[
            "api",
            "-X",
            "PUT",
            "/repos/o/r/branches/main/protection",
            "-f",
            "enforce_admins=true",
        ]);
        assert!(matches!(
            classify(&field_args, &admin_manifest, "macos"),
            Classification::Admin { ref tuple } if tuple == BRANCH_PROTECTION_API_TUPLE
        ));
    }

    #[test]
    fn delete_branch_protection_is_admin_only_when_declared_and_not_destructive() {
        let args = os_args(&["api", "-X", "DELETE", "/repos/o/r/branches/main/protection"]);
        assert!(matches!(
            classify(
                &args,
                &branch_protection_manifest("DELETE", Tier::Admin),
                "macos"
            ),
            Classification::Admin { ref tuple }
                if tuple == "api:DELETE:/repos/*/*/branches/*/protection"
        ));
        assert!(matches!(
            classify(&args, &v12_fixture_manifest(), "macos"),
            Classification::Unclassified
        ));
    }

    /// `gh api repos/o/r/...` (no leading slash) is the everyday spelling and
    /// the same request as `/repos/o/r/...`; a declared endpoint must classify
    /// identically under both, or the common form refuses as undeclared.
    #[test]
    fn slashless_api_endpoint_classifies_like_the_declared_glob() {
        let manifest = branch_protection_manifest("PUT", Tier::Admin);
        for spelling in [
            "/repos/o/r/branches/main/protection",
            "repos/o/r/branches/main/protection",
        ] {
            let args = os_args(&["api", "-X", "PUT", spelling, "--input", "-"]);
            assert!(
                matches!(
                    classify(&args, &manifest, "macos"),
                    Classification::Admin { ref tuple } if tuple == BRANCH_PROTECTION_API_TUPLE
                ),
                "{spelling} must classify as the declared admin endpoint"
            );
        }
        // An undeclared endpoint stays undeclared under either spelling.
        let args = os_args(&["api", "-X", "PUT", "repos/o/r/topics", "--input", "-"]);
        assert!(matches!(
            classify(&args, &manifest, "macos"),
            Classification::Unclassified
        ));
    }

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn target_repository_reads_positional_urls_and_endpoints_but_not_flag_values() {
        let named = |raw: &[&str]| positional_target_repository(&os_args(raw));
        assert_eq!(
            named(&[
                "issue",
                "comment",
                "https://github.com/Owner/Repo/issues/5#issuecomment-1",
                "--body",
                "x"
            ]),
            Some("owner/repo".to_string())
        );
        assert_eq!(
            named(&[
                "pr",
                "review",
                "--comment",
                "https://github.com/o/r/pull/7",
                "-b",
                "x"
            ]),
            Some("o/r".to_string())
        );
        // A URL given as the body is prose, not the target.
        assert_eq!(
            named(&[
                "issue",
                "comment",
                "5",
                "--body",
                "https://github.com/o/r/issues/1"
            ]),
            None
        );
        assert_eq!(
            named(&[
                "pr",
                "close",
                "7",
                "--comment",
                "https://github.com/o/r/pull/1"
            ]),
            None
        );
        assert_eq!(
            named(&[
                "api",
                "-X",
                "PATCH",
                "repos/o/r/issues/comments/1",
                "-f",
                "body=x"
            ]),
            Some("o/r".to_string())
        );
        // Placeholders are filled from the other sources by upstream gh.
        assert_eq!(named(&["api", "repos/{owner}/{repo}/issues"]), None);
        // `gh repo` subcommands name their repository as the first positional,
        // past any flag value that happens to look like `owner/name`.
        assert_eq!(
            named(&["repo", "delete", "Owner/Repo", "--yes"]),
            Some("owner/repo".to_string())
        );
        assert_eq!(
            named(&[
                "repo",
                "create",
                "--description",
                "a/b",
                "cortexkit/common-auth",
                "--private"
            ]),
            Some("cortexkit/common-auth".to_string())
        );
        assert_eq!(
            named(&["repo", "fork", "https://github.com/o/r", "--clone"]),
            Some("o/r".to_string())
        );
        // A bare name has no owner; `repo rename` takes the new name, not the
        // repository; a key path is not a repository.
        assert_eq!(named(&["repo", "create", "common-auth"]), None);
        assert_eq!(named(&["repo", "rename", "o/new"]), None);
        assert_eq!(named(&["repo", "deploy-key", "add", "keys/id.pub"]), None);

        let explicit = explicit_repo(&os_args(&["issue", "comment", "5", "-R=o/r"]));
        assert_eq!(explicit.as_deref(), Some("o/r"));
        let target = TargetRepository {
            explicit: Some("o/explicit".to_string()),
            url: Some("o/url".to_string()),
            gh_repo: Some("o/env".to_string()),
        };
        assert_eq!(target.named(), Some("o/explicit"));
        let target = TargetRepository {
            explicit: None,
            ..target
        };
        assert_eq!(target.named(), Some("o/url"));
        let target = TargetRepository {
            url: None,
            ..target
        };
        assert_eq!(target.named(), Some("o/env"));
        // A named repository on another host never falls back to the origin.
        let foreign = TargetRepository {
            explicit: Some("ghe.example.com/o/r".to_string()),
            ..TargetRepository::default()
        };
        assert_eq!(foreign.repository_key(Path::new(".")), None);
    }

    #[test]
    fn unbound_safe_list_passes_reads_and_local_commands_and_refuses_everything_else() {
        let safe = |raw: &[&str]| is_unbound_safe(&os_args(raw));
        for raw in [
            // Writes the manifest can declare: bot speech (comments, issue
            // creation), operator administration and label changes.
            &["issue", "comment", "5", "--body", "x"][..],
            &["issue", "create", "--title", "t"],
            &["pr", "merge", "7"],
            &["release", "create", "v1"],
            &["label", "create", "bug"],
            &["workflow", "run", "ci.yml"],
            &["repo", "edit", "--visibility", "public"],
            // Writes no manifest declares, and account writes.
            &["repo", "create", "cortexkit/common-auth", "--private"],
            &["repo", "delete", "o/r", "--yes"],
            &["repo", "fork", "o/r"],
            &["pr", "create", "--fill"],
            &["gist", "create", "notes.md"],
            &["secret", "set", "TOKEN"],
            &["repo", "deploy-key", "add", "key.pub"],
            // Verbs the shim has no table entry for: refused, not assumed
            // to be reads.
            &["codespace", "create", "-R", "o/r"],
            &["agent-task", "create", "fix the build"],
            &["extension", "install", "owner/gh-ext"],
            // Extensions and aliases run code the shim cannot inspect.
            &["extension", "exec", "gh-ext"],
            &["my-extension", "--flag"],
            &["co", "7"],
            // Browsing opens a browser unless asked only for the URL.
            &["browse", "12"],
            &["browse", "12", "extra", "--no-browser"],
            // The operator's credentials: printing the token, or changing
            // the login or git's credential configuration.
            &["auth", "token"],
            &["auth", "status", "--show-token"],
            &["auth", "status", "-t"],
            &["auth", "status", "-at"],
            &["auth", "login", "--with-token"],
            &["auth", "logout"],
            &["auth", "refresh"],
            &["auth", "switch"],
            &["auth", "setup-git"],
            // Subcommands of safe verbs that this build does not list, as a
            // future gh might add them.
            &["config", "future-subcommand"],
            &["alias", "future-subcommand"],
            &["search", "future-kind", "x"],
            &["status", "future-subcommand"],
            &["version", "future-subcommand"],
            &["completion", "-s", "future-shell"],
            &["help", "future-command"],
            // A flag before the verb that the shim does not model: its value
            // may be what reads as the verb.
            &["--future-global", "search", "issues", "flaky"],
            &["--future-global", "value", "issue", "view", "5"],
            // API writes: a named method, or a payload without one.
            &["api", "-X", "POST", "repos/o/r/issues"],
            &["api", "--method=delete", "repos/o/r"],
            &["api", "-XDELETE", "repos/o/r"],
            &["api", "repos/o/r/issues/1/comments", "-f", "body=x"],
            &["api", "user/repos", "--input", "repo.json"],
            &["api", "graphql", "-f", "query=mutation { addStar }"],
            &["api", "graphql", "-F", "query=@query.graphql"],
        ] {
            assert!(!safe(raw), "{raw:?} must not pass an unbound target");
        }
        for raw in [
            // Reads.
            &["issue", "view", "5"][..],
            &["issue", "list"],
            &["pr", "list"],
            &["run", "view", "1", "--log"],
            &["run", "download", "1"],
            &["release", "download", "v1"],
            &["repo", "view", "o/r"],
            &["repo", "deploy-key", "list"],
            &["search", "issues", "flaky"],
            &["search", "prs", "--author", "me"],
            &["status"],
            &["gist", "view", "abc"],
            &["extension", "list"],
            // Local machine only.
            &["auth", "status"],
            &["auth", "status", "--show-token=false"],
            &["config", "set", "editor", "vim"],
            &["config", "get", "editor"],
            &["alias", "set", "co", "pr checkout"],
            &["alias", "list"],
            &["completion", "-s", "zsh"],
            &["help", "repo"],
            &["help"],
            &["version"],
            &["repo", "clone", "o/r"],
            &["pr", "checkout", "7"],
            &["browse", "12", "--no-browser"],
            &["repo", "create", "--help"],
            &[],
            &["--version"],
            // API reads.
            &["api", "repos/o/r/issues"],
            &["api", "-X", "GET", "search/issues", "-f", "q=repo:o/r"],
            &["api", "graphql", "-f", "query={ viewer { login } }"],
        ] {
            assert!(safe(raw), "{raw:?} should pass through");
        }
        // A method attached to the flag (`-XDELETE`) is read as the method
        // by classification as well, so a field-free DELETE is not taken for
        // the default GET, which the manifest's `GET **` rule passes through.
        assert!(matches!(
            classify(
                &os_args(&["api", "-XDELETE", "repos/o/r"]),
                &fixture_manifest(),
                "macos"
            ),
            Classification::Unclassified
        ));
    }

    #[test]
    fn auth_commands_that_reveal_or_change_the_operators_credentials_are_named() {
        let credential = |raw: &[&str]| operator_credential_use(&os_args(raw));
        assert_eq!(
            credential(&["auth", "token"]),
            Some(("auth token".to_string(), CredentialUse::RevealsToken))
        );
        assert_eq!(
            credential(&["auth", "status", "-h", "github.com", "--show-token"]),
            Some(("auth status".to_string(), CredentialUse::RevealsToken))
        );
        for subcommand in [
            "login",
            "logout",
            "refresh",
            "switch",
            "setup-git",
            "future",
        ] {
            assert_eq!(
                credential(&["auth", subcommand]),
                Some((
                    format!("auth {subcommand}"),
                    CredentialUse::ChangesCredentials
                ))
            );
        }
        for raw in [
            &["auth", "status"][..],
            &["auth", "status", "-h", "github.com", "--json", "hosts"],
            &["auth"],
            &["auth", "token", "--help"],
            &["issue", "view", "5"],
        ] {
            assert_eq!(credential(raw), None, "{raw:?}");
        }
        assert_eq!(
            operator_credentials_refusal_text("auth token", CredentialUse::RevealsToken),
            "`auth token` prints the operator's GitHub token into this agent's session, and with it an agent could call the GitHub API directly, around the shim. The operator can approve it: re-run with GH_SHIM_BYPASS=operator, and the shim records an operator-attributed audit line."
        );
    }

    /// State for a local `gh auth status` answer: a state directory, a
    /// connection file that is only a placeholder (nothing listens behind
    /// it, so any probe would fail), and the user config naming it.
    struct AuthStatusFixture {
        _directory: tempfile::TempDir,
        paths: StatePaths,
        config_doc: String,
    }

    impl AuthStatusFixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("create auth status state directory");
            let paths = StatePaths::from_root(directory.path().join("state"));
            let connection_file = directory.path().join("subc-connection.json");
            fs::write(&connection_file, b"{}").expect("write placeholder connection file");
            let config_doc = json!({ "subc": { "connection_file": connection_file } }).to_string();
            Self {
                _directory: directory,
                paths,
                config_doc,
            }
        }

        fn record_rung(&self, rung: Rung, inputs: &[(&str, &str)]) {
            fs::create_dir_all(&self.paths.root).expect("state root");
            let record = RungRecord {
                rung,
                as_of_unix_secs: TEST_NOW - 30,
                inputs: inputs
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect(),
                manifest_version: Some(12),
                recorded_by_image_path: None,
                recorded_by_version: None,
                recorded_by_repo_key: None,
                last_reachable_unix_secs: None,
            };
            fs::write(&self.paths.rung, serde_json::to_vec(&record).unwrap()).unwrap();
        }

        fn record_r3(&self) {
            self.record_rung(
                Rung::R3,
                &[
                    ("connection_file", "ready"),
                    ("catalog_gh_route", "ready"),
                    ("agent_credentials_present", "absent"),
                ],
            );
        }

        fn answer(&self, raw: &[&str], repository: Option<&str>) -> AuthStatusAnswer {
            answer_auth_status(
                &os_args(raw),
                &self.paths,
                TEST_NOW,
                Some(&self.config_doc),
                || repository.map(str::to_string),
            )
        }

        /// Dispatch through the same seam `run` uses, failing the test if
        /// the real `gh` would have been spawned.
        fn dispatch_without_upstream(&self, raw: &[&str], repository: Option<&str>) -> i32 {
            dispatch_auth_status(
                &os_args(raw),
                &self.paths,
                TEST_NOW,
                Some(&self.config_doc),
                || repository.map(str::to_string),
                |_| panic!("`gh auth status` ran the real gh"),
            )
        }
    }

    fn auth_status_report(answer: AuthStatusAnswer) -> (String, i32) {
        match answer {
            AuthStatusAnswer::Report { text, exit_code } => (text, exit_code),
            other => panic!("expected a local auth status report, got {other:?}"),
        }
    }

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn auth_status_names_the_bound_bot_and_exits_zero_from_local_state_only() {
        let fixture = AuthStatusFixture::new();
        write_signed_manifest(&fixture.paths, v12_fixture_manifest(), TEST_NOW);
        fixture.record_r3();
        let rung_before = fs::read(&fixture.paths.rung).unwrap();

        for raw in [
            &["auth", "status"][..],
            &["auth", "status", "-h", "github.com"],
            &["auth", "status", "--hostname", "github.com"],
            &["auth", "status", "--hostname=github.com"],
        ] {
            let (text, exit_code) = auth_status_report(fixture.answer(raw, Some("cortexkit/aft")));
            assert_eq!(
                text,
                "github.com (answered by the AFT gh shim from local state; the real gh was not run)\n  Repository: cortexkit/aft\n  \u{2713} Governed writes: as alfonso-aft (the signed routing manifest binds cortexkit/aft to it)\n  - Routing manifest: version 12, signature verified\n  - Governed routing: ready (last rung R3, recorded 30s ago; connection file present)\n  - Reads: run by the real gh under the operator's own gh login, not the bot identity\n",
                "{raw:?}"
            );
            assert_eq!(exit_code, 0, "{raw:?}");
            assert_eq!(
                fixture.dispatch_without_upstream(raw, Some("cortexkit/aft")),
                0
            );
        }
        // Nothing probed the daemon: a probe records its stage and refreshes
        // the rung record, and neither happened.
        assert!(!fixture.paths.last_probe.exists());
        assert_eq!(fs::read(&fixture.paths.rung).unwrap(), rung_before);
        assert!(!fixture.paths.bypass_audit.exists());
    }

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn auth_status_on_an_unbound_repository_names_the_operator_bypass_and_exits_one() {
        let fixture = AuthStatusFixture::new();
        write_signed_manifest(&fixture.paths, v12_fixture_manifest(), TEST_NOW);
        fixture.record_r3();

        let (text, exit_code) =
            auth_status_report(fixture.answer(&["auth", "status"], Some("earendil-works/pi")));
        assert_eq!(exit_code, 1);
        assert_eq!(
            text,
            "github.com (answered by the AFT gh shim from local state; the real gh was not run)\n  Repository: earendil-works/pi\n  X Governed writes: none (earendil-works/pi is unbound; writes are refused unless the operator bypass applies)\n  - Routing manifest: version 12, signature verified\n  - Governed routing: ready (last rung R3, recorded 30s ago; connection file present)\n  - Reads: run by the real gh under the operator's own gh login, not the bot identity\nGoverned writes unavailable: earendil-works/pi is not bound to a bot in the signed routing manifest, so writes there are refused unless the operator approves them with GH_SHIM_BYPASS=operator\n"
        );
        assert_eq!(
            fixture.dispatch_without_upstream(&["auth", "status"], Some("earendil-works/pi")),
            1
        );
    }

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn auth_status_outside_a_repository_or_without_routing_names_the_reason_and_exits_one() {
        let fixture = AuthStatusFixture::new();
        write_signed_manifest(&fixture.paths, v12_fixture_manifest(), TEST_NOW);

        // No rung recorded yet: routing is unconfirmed, and nothing probes.
        let (text, exit_code) =
            auth_status_report(fixture.answer(&["auth", "status"], Some("cortexkit/aft")));
        assert_eq!(exit_code, 1);
        assert!(text.contains("\u{2713} Governed writes: as alfonso-aft"));
        assert!(text.ends_with("Governed writes unavailable: governed routing has not been confirmed on this machine yet (no rung recorded); the next governed write probes the governance daemon\n"), "{text}");
        assert!(!fixture.paths.last_probe.exists());

        // The last rung fell short of R3: its cause is named, while the value
        // naming where an ambient credential was found is not printed.
        fixture.record_rung(
            Rung::R2,
            &[
                ("connection_file", "ready"),
                ("agent_credentials_present", "env:GH_TOKEN"),
            ],
        );
        let (text, exit_code) =
            auth_status_report(fixture.answer(&["auth", "status"], Some("cortexkit/aft")));
        assert_eq!(exit_code, 1);
        assert!(text.contains("  - Governed routing: unavailable (last rung R2: agent_credentials_present, recorded 30s ago; connection file present)\n"), "{text}");
        assert!(text.ends_with("Governed writes unavailable: governed routing is unavailable: the last rung recorded was R2 (agent_credentials_present), not R3\n"), "{text}");
        assert!(!text.contains("GH_TOKEN"), "{text}");

        // Outside any repository there is no repository-to-bot binding to name.
        fixture.record_r3();
        let (text, exit_code) = auth_status_report(fixture.answer(&["auth", "status"], None));
        assert_eq!(exit_code, 1);
        assert!(
            text.contains("  Repository: none\n  X Governed writes: none (no repository)\n"),
            "{text}"
        );
        assert!(text.ends_with("Governed writes unavailable: no repository: this directory has no github.com origin remote and GH_REPO is unset, so no bot binding applies here (a write that names a bound repository with --repo uses that repository's bot)\n"), "{text}");

        // No connection file configured: routing cannot be reached at all.
        let (text, exit_code) = auth_status_report(answer_auth_status(
            &os_args(&["auth", "status"]),
            &fixture.paths,
            TEST_NOW,
            Some("{}"),
            || Some("cortexkit/aft".to_string()),
        ));
        assert_eq!(exit_code, 1);
        assert!(text.ends_with("Governed writes unavailable: governed routing is not configured: the user aft.jsonc names no subc.connection_file\n"), "{text}");
    }

    #[test]
    fn auth_status_with_an_invalid_manifest_names_the_failure_and_exits_one() {
        let fixture = AuthStatusFixture::new();
        write_envelope_fixture(&fixture.paths, "not a signed manifest");
        fixture.record_r3();

        let (text, exit_code) =
            auth_status_report(fixture.answer(&["auth", "status"], Some("cortexkit/aft")));
        assert_eq!(exit_code, 1);
        assert!(text.contains("  X Governed writes: none (the routing manifest did not verify)\n  - Routing manifest: failed verification: invalid ("), "{text}");
        assert!(text.contains("\nGoverned writes unavailable: the installed routing manifest failed verification (invalid ("), "{text}");
        assert_eq!(
            fixture.dispatch_without_upstream(&["auth", "status"], Some("cortexkit/aft")),
            1
        );
    }

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn auth_status_with_a_regressed_manifest_refuses_the_cached_binding() {
        let fixture = AuthStatusFixture::new();
        write_signed_manifest(&fixture.paths, v12_fixture_manifest(), TEST_NOW);
        fixture.record_r3();
        // Accept the valid manifest once so a last-valid copy is cached, then
        // replace it with one that fails verification.
        assert!(matches!(
            resolve_manifest(&fixture.paths, TEST_NOW),
            ManifestResolution::Active(_)
        ));
        write_envelope_fixture(&fixture.paths, "not a signed manifest");

        let (text, exit_code) =
            auth_status_report(fixture.answer(&["auth", "status"], Some("cortexkit/aft")));
        assert_eq!(exit_code, 1);
        assert!(!text.contains("as alfonso-aft"), "{text}");
        assert!(text.contains("(last valid version 12 is cached)"), "{text}");
        assert!(
            text.contains("so governed writes are refused until a valid manifest is installed\n"),
            "{text}"
        );
    }

    #[test]
    fn auth_status_passes_through_when_no_manifest_is_installed_or_the_shim_is_off() {
        use std::cell::Cell;

        // Dormant: no manifest, a public installation. The real `gh` answers
        // with the user's own login, and every form passes through unread.
        let fixture = AuthStatusFixture::new();
        for raw in [&["auth", "status"][..], &["auth", "status", "--active"]] {
            assert_eq!(
                fixture.answer(raw, Some("cortexkit/aft")),
                AuthStatusAnswer::PassThrough
            );
        }
        let delegated = Cell::new(0);
        let status = dispatch_auth_status(
            &os_args(&["auth", "status"]),
            &fixture.paths,
            TEST_NOW,
            Some(&fixture.config_doc),
            || panic!("a pass-through resolves no repository"),
            |ran| {
                assert_eq!(ran, os_args(&["auth", "status"]));
                delegated.set(delegated.get() + 1);
                73
            },
        );
        assert_eq!((status, delegated.get()), (73, 1));

        // Operator hard-off: byte-transparent pass-through even though an
        // installed manifest would otherwise be answered locally.
        write_envelope_fixture(&fixture.paths, "not a signed manifest");
        assert!(matches!(
            fixture.answer(&["auth", "status"], Some("cortexkit/aft")),
            AuthStatusAnswer::Report { .. }
        ));
        let disabled = json!({ "github": { "shim": false } }).to_string();
        assert_eq!(
            answer_auth_status(
                &os_args(&["auth", "status"]),
                &fixture.paths,
                TEST_NOW,
                Some(&disabled),
                || panic!("a pass-through resolves no repository"),
            ),
            AuthStatusAnswer::PassThrough
        );
    }

    #[test]
    fn auth_status_token_forms_stay_refused_and_unknown_flags_are_refused_by_name() {
        // `-t`, `--show-token` and their spellings never reach the local
        // answer: they keep the existing refusal for commands that would
        // print the operator's token. `--help` stays with the real gh.
        for raw in [
            &["auth", "status", "-t"][..],
            &["auth", "status", "--show-token"],
            &["auth", "status", "-at"],
            &["auth", "status", "-h", "github.com", "--show-token"],
        ] {
            assert!(!is_local_auth_status(&os_args(raw)), "{raw:?}");
            assert_eq!(
                operator_credential_use(&os_args(raw)),
                Some(("auth status".to_string(), CredentialUse::RevealsToken)),
                "{raw:?}"
            );
        }
        for raw in [
            &["auth", "status", "--help"][..],
            &["auth", "token"],
            &["auth"],
            &["issue", "status"],
        ] {
            assert!(!is_local_auth_status(&os_args(raw)), "{raw:?}");
        }
        assert!(is_local_auth_status(&os_args(&["auth", "status"])));
        assert!(is_local_auth_status(&os_args(&[
            "auth", "status", "--json", "hosts"
        ])));

        let refusal = |raw: &[&str]| auth_status_argument_refusal(&os_args(raw));
        for raw in [
            &["auth", "status"][..],
            &["auth", "status", "-h", "github.com"],
            &["auth", "status", "--hostname", "GitHub.com"],
            &["auth", "status", "--hostname=github.com"],
        ] {
            assert_eq!(refusal(raw), None, "{raw:?}");
        }
        let answered = "the shim answers `gh auth status` itself, optionally with `--hostname github.com`, and does not pass other forms to the real gh, whose answer would describe the operator's login rather than the identity governed writes use";
        for (raw, prefix) in [
            (
                &["auth", "status", "--json", "hosts"][..],
                "`auth status --json` is not supported by the shim",
            ),
            (
                &["auth", "status", "--json=hosts"],
                "`auth status --json` is not supported by the shim",
            ),
            (
                &["auth", "status", "-a"],
                "`auth status -a` is not supported by the shim",
            ),
            (
                &["auth", "status", "--active"],
                "`auth status --active` is not supported by the shim",
            ),
            (
                &["auth", "status", "-h", "ghe.example.com"],
                "`auth status --hostname ghe.example.com` asks about a host the shim does not govern (it governs github.com only)",
            ),
            (
                &["auth", "status", "-h"],
                "`auth status -h` needs a host name",
            ),
            (
                &["auth", "status", "extra"],
                "`auth status` takes no argument, but was given `extra`",
            ),
        ] {
            assert_eq!(refusal(raw), Some(format!("{prefix}; {answered}")), "{raw:?}");
        }

        // With governance in force the refusal happens without running gh.
        let fixture = AuthStatusFixture::new();
        write_envelope_fixture(&fixture.paths, "not a signed manifest");
        assert_eq!(
            fixture.dispatch_without_upstream(&["auth", "status", "--json", "hosts"], None),
            REFUSAL_EXIT_STATUS
        );
    }

    /// The target as `TargetRepository::from_invocation` reads it, minus the
    /// process `GH_REPO`, so these tests do not depend on the environment.
    fn target_of(args: &[OsString]) -> TargetRepository {
        TargetRepository {
            explicit: explicit_repo(args),
            url: positional_target_repository(args),
            gh_repo: None,
        }
    }

    #[test]
    fn write_target_names_the_repository_the_account_or_why_it_is_undetermined() {
        let outside = tempfile::tempdir().expect("create a directory outside any repository");
        let target = |raw: &[&str]| {
            let args = os_args(raw);
            write_target(&args, &target_of(&args), outside.path())
        };
        assert_eq!(
            target(&[
                "issue",
                "comment",
                "5",
                "-R",
                "earendil-works/pi",
                "-b",
                "x"
            ]),
            WriteTarget::Repository("earendil-works/pi".to_string())
        );
        assert_eq!(
            target(&["repo", "create", "cortexkit/common-auth", "--private"]),
            WriteTarget::NotARepository {
                description: "creates the new repository cortexkit/common-auth".to_string(),
                named: Some("cortexkit/common-auth".to_string()),
            }
        );
        assert_eq!(
            target(&["repo", "create", "common-auth"]),
            WriteTarget::NotARepository {
                description: "creates a new repository".to_string(),
                named: None,
            }
        );
        assert_eq!(
            target(&["api", "-X", "POST", "/user/repos", "-f", "name=x"]),
            WriteTarget::NotARepository {
                description: "calls /user/repos, an endpoint outside /repos/<owner>/<repo>"
                    .to_string(),
                named: None,
            }
        );
        assert!(matches!(
            target(&["issue", "comment", "5", "--body", "x"]),
            WriteTarget::Undetermined(reason) if reason.contains("no github.com origin remote")
        ));
        assert!(matches!(
            target(&["issue", "comment", "5", "-R", "ghe.example.com/o/r", "-b", "x"]),
            WriteTarget::Undetermined(reason) if reason.contains("`ghe.example.com/o/r` is not a github.com owner/name repository")
        ));
    }

    #[test]
    fn unbound_write_leaves_bound_targets_reads_and_destructive_forms_to_the_governed_path() {
        let outside = tempfile::tempdir().expect("create a directory outside any repository");
        let manifest = v12_fixture_manifest();
        let unbound = |raw: &[&str]| {
            let args = os_args(raw);
            unbound_write(&args, &manifest, "macos", &target_of(&args), outside.path())
        };
        // `cortexkit/aft` is the fixture's one bound repository.
        assert_eq!(
            unbound(&["issue", "comment", "5", "-R", "cortexkit/aft", "-b", "x"]),
            None
        );
        assert_eq!(unbound(&["pr", "merge", "7", "-R", "cortexkit/aft"]), None);
        assert_eq!(
            unbound(&["issue", "view", "5", "-R", "earendil-works/pi"]),
            None
        );
        assert_eq!(unbound(&["api", "repos/earendil-works/pi/issues"]), None);
        assert_eq!(
            unbound(&["release", "delete", "v1", "-R", "earendil-works/pi"]),
            None
        );

        let comment = unbound(&[
            "issue",
            "comment",
            "5",
            "-R",
            "earendil-works/pi",
            "-b",
            "x",
        ])
        .expect("a comment on an unbound repository is an unbound write");
        assert_eq!(comment.command, "issue comment");
        assert_eq!(
            unbound_target_refusal_text(&comment),
            "`issue comment` targets earendil-works/pi, which is not a bot-bound repository (the signed gh routing manifest binds no bot to it); bot speech is not possible there, and upstream gh would run it under the operator's own login. The operator can approve it: re-run with GH_SHIM_BYPASS=operator, and the shim records an operator-attributed audit line."
        );
        let create = unbound(&["repo", "create", "cortexkit/common-auth", "--private"])
            .expect("creating a repository is an unbound write");
        assert_eq!(create.command, "repo create");
        assert_eq!(
            create.target.audit_repository(),
            Some("cortexkit/common-auth")
        );
        let api = unbound(&["api", "-X", "POST", "repos/earendil-works/pi/issues"])
            .expect("an API write on an unbound repository is an unbound write");
        assert_eq!(api.command, "api:POST:/repos/earendil-works/pi/issues");
        assert_eq!(
            api.target,
            WriteTarget::Repository("earendil-works/pi".to_string())
        );
        let undetermined = unbound(&["issue", "comment", "5", "--body", "x"])
            .expect("a write with no determinable target is refused");
        assert!(unbound_target_refusal_text(&undetermined).starts_with(
            "`issue comment` has no determinable target repository (no --repo, repository URL or GH_REPO names one, and the working directory has no github.com origin remote)"
        ));
    }

    #[test]
    fn unbound_write_refuses_without_the_bypass_and_audits_before_running_with_it() {
        use std::cell::Cell;

        let _env_lock = crate::test_env::process_env_lock();
        let directory = tempfile::tempdir().expect("create unbound write state directory");
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        let args = os_args(&["repo", "create", "cortexkit/common-auth", "--private"]);
        let write = UnboundWrite {
            command: "repo create".to_string(),
            target: WriteTarget::NotARepository {
                description: "creates the new repository cortexkit/common-auth".to_string(),
                named: Some("cortexkit/common-auth".to_string()),
            },
        };

        {
            let _bypass = ScopedTestEnvVar::set("GH_SHIM_BYPASS", None);
            let status = dispatch_unbound_write(&args, &write, &paths, TEST_NOW, |_| {
                panic!("an unbound write ran without the operator bypass")
            });
            assert_eq!(status, REFUSAL_EXIT_STATUS);
            assert!(!paths.bypass_audit.exists());
        }

        let delegated = Cell::new(0);
        {
            let _bypass = ScopedTestEnvVar::set("GH_SHIM_BYPASS", Some("operator"));
            let status = dispatch_unbound_write(&args, &write, &paths, TEST_NOW, |ran| {
                assert_eq!(ran, args);
                // The audit line is durable before upstream `gh` is spawned.
                assert!(paths.bypass_audit.exists());
                delegated.set(delegated.get() + 1);
                73
            });
            assert_eq!(status, 73);
        }
        assert_eq!(delegated.get(), 1);
        let (records, error) = read_bypass_audit(&paths);
        assert!(error.is_none());
        assert_eq!(
            records.expect("operator bypass audit records"),
            vec![json!({
                "as_of_unix_secs": TEST_NOW,
                "tuple": "repo create",
                "repository": "cortexkit/common-auth",
            })]
        );
    }

    /// The binding and the canonical request must name the same repository;
    /// otherwise the request would speak in one repository as another's bot.
    #[test]
    fn governed_request_for_a_repository_other_than_the_binding_refuses_before_routing() {
        let _env_lock = crate::test_env::process_env_lock();
        let directory = tempfile::tempdir().expect("create mismatch state directory");
        // No user config under this HOME, so a request that got past the
        // check could not reach any real governance daemon.
        let _home = ScopedTestEnvVar::set("HOME", Some(directory.path().to_str().unwrap()));
        let _config = ScopedTestEnvVar::set(
            "XDG_CONFIG_HOME",
            Some(directory.path().join("config").to_str().unwrap()),
        );
        let _gh_repo = ScopedTestEnvVar::set("GH_REPO", None);
        let paths = StatePaths::from_root(directory.path().join("state"));
        let manifest = v12_fixture_manifest();
        let rung =
            RungDetermination::r3(TEST_NOW, manifest.manifest_version, &test_rung_provenance())
                .record;
        let binding = AgentBinding {
            repo: "cortexkit/magic-context".to_string(),
            agent_id: "alfonso-magic-context".to_string(),
        };
        let args = os_args(&[
            "issue",
            "comment",
            "42",
            "-R",
            "cortexkit/aft",
            "--body",
            "hello",
        ]);
        let classification = classify(&args, &manifest, "macos");
        assert!(matches!(classification, Classification::Governed { .. }));
        let status = dispatch_r3(
            &args,
            classification,
            &manifest,
            &paths,
            &rung,
            &binding,
            TEST_NOW,
            |_| panic!("a governed request reached upstream gh"),
        );
        assert_eq!(status, REFUSAL_EXIT_STATUS);
        // Routing records the binding in the seam state before it dials, so
        // an absent file shows the refusal came before any route attempt.
        assert!(
            !paths.seam_state.exists(),
            "the mismatched request was sent toward the route"
        );
    }

    #[test]
    fn dev_signed_admin_api_dispatch_requires_bypass_and_audits_delegation() {
        use std::cell::Cell;

        let _env_lock = crate::test_env::process_env_lock();
        let directory = tempfile::tempdir().expect("create admin dispatch state directory");
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        // The rule is declared for the platforms the fleet runs on; this test
        // exercises dispatch given an Admin classification, so it classifies
        // against a declared platform rather than the host (Windows would
        // classify nothing and assert the refusal arm twice).
        let manifest = branch_protection_manifest("PUT", Tier::Admin);
        manifest.validate().expect("valid admin API manifest");
        write_signed_manifest(&paths, manifest, TEST_NOW);
        let manifest = load_manifest(&paths, TEST_NOW).expect("dev-signed manifest verifies");
        let args = os_args(&[
            "api",
            "-X",
            "PUT",
            "/repos/o/r/branches/main/protection",
            "--input",
            "body.json",
        ]);
        let rung =
            RungDetermination::r3(TEST_NOW, manifest.manifest_version, &test_rung_provenance())
                .record;
        let binding = AgentBinding {
            repo: "cortexkit/aft".to_string(),
            agent_id: "alfonso-aft".to_string(),
        };

        {
            let _bypass = ScopedTestEnvVar::set("GH_SHIM_BYPASS", None);
            let status = dispatch_r3(
                &args,
                classify(&args, &manifest, "macos"),
                &manifest,
                &paths,
                &rung,
                &binding,
                TEST_NOW,
                |_| panic!("ADMIN request delegated without operator bypass"),
            );
            assert_eq!(status, REFUSAL_EXIT_STATUS);
            assert_eq!(RefusalCode::AdminTier.as_str(), "gh_shim_admin_tier");
        }

        let delegated = Cell::new(0);
        {
            let _bypass = ScopedTestEnvVar::set("GH_SHIM_BYPASS", Some("operator"));
            let status = dispatch_r3(
                &args,
                classify(&args, &manifest, "macos"),
                &manifest,
                &paths,
                &rung,
                &binding,
                TEST_NOW,
                |delegated_args| {
                    assert_eq!(delegated_args, args);
                    delegated.set(delegated.get() + 1);
                    73
                },
            );
            assert_eq!(status, 73);
        }
        assert_eq!(delegated.get(), 1);
        let (records, error) = read_bypass_audit(&paths);
        assert!(error.is_none());
        let records = records.expect("operator bypass audit records");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["tuple"], BRANCH_PROTECTION_API_TUPLE);
    }

    #[test]
    fn release_api_mutations_remain_unclassified_because_api_rules_are_get_only() {
        let manifest = v12_fixture_manifest();
        // The v1 audit keeps api_rules GET-only; do not widen them for REST writes.
        for args in [
            os_args(&["api", "-X", "PATCH", "repos/owner/repo/releases/42"]),
            os_args(&["api", "-X", "POST", "repos/owner/repo/releases/42/assets"]),
        ] {
            assert!(matches!(
                classify(&args, &manifest, "macos"),
                Classification::Unclassified
            ));
        }
    }

    #[test]
    fn field_bearing_get_remains_unclassified_without_widening_the_mechanical_fallback() {
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
        assert!(matches!(
            classify(
                &os_args(&[
                    "api",
                    "/repos/o/r/branches/main/protection",
                    "--input",
                    "body.json"
                ]),
                &manifest,
                "macos"
            ),
            Classification::Unclassified
        ));
        assert!(matches!(
            classify(
                &os_args(&["api", "/repos/o/r/branches/main/protection"]),
                &manifest,
                "macos"
            ),
            Classification::Mechanical
        ));
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

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
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

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
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

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
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

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
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

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
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

    fn retained_files(paths: &StatePaths) -> Vec<PathBuf> {
        fs::read_dir(&paths.manifests_dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn activation_retains_every_accepted_manifest_with_exact_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());

        // Activate v11 then v12 in the same temp state dir.
        write_signed_manifest(&paths, v11_fixture_manifest(), TEST_NOW);
        assert_eq!(
            load_manifest(&paths, TEST_NOW).unwrap().manifest_version,
            11
        );
        write_signed_manifest(&paths, v12_fixture_manifest(), TEST_NOW);
        assert_eq!(
            load_manifest(&paths, TEST_NOW).unwrap().manifest_version,
            12
        );

        // Two files, one per accepted version.
        let files = retained_files(&paths);
        assert_eq!(files.len(), 2);

        // Reading each back and re-verifying its signature passes, and the
        // payload bytes are the exact signed bytes (never a re-serialization).
        let mut versions = BTreeSet::new();
        for file in &files {
            let record: RetainedManifest =
                serde_json::from_slice(&fs::read(file).unwrap()).expect("retained record");
            let envelope = SignedManifest {
                artifact_id: MANIFEST_ARTIFACT_ID.to_string(),
                envelope_version: ENVELOPE_VERSION,
                key_id: record.key_id.clone(),
                fetched_at_unix_secs: TEST_NOW,
                signature: record.signature.clone(),
                manifest_bytes: record.manifest_bytes.clone(),
            };
            let verified =
                verify_manifest_signature(&envelope).expect("retained signature re-verifies");
            versions.insert(verified.manifest_version);
        }
        assert_eq!(versions, BTreeSet::from([11, 12]));

        // The self-report reflects the retained count and directory.
        let document = render_self_report(&paths).expect("self report");
        let value: Value = serde_json::from_str(&document).expect("self report JSON");
        assert_eq!(value["manifests_retained"], json!(2));
        assert_eq!(
            value["manifests_dir"],
            json!(paths.manifests_dir.to_string_lossy())
        );
    }

    #[test]
    fn tampered_payload_is_not_retained() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());

        // A tampered envelope fails signature verification, so activation is
        // refused and nothing is filed.
        write_envelope_fixture(
            &paths,
            include_str!("../tests/fixtures/gh_shim/signed-envelope-v2-tampered.json"),
        );
        assert!(load_manifest(&paths, TEST_NOW).is_err());
        assert!(retained_files(&paths).is_empty());
    }

    #[test]
    fn version_mismatch_between_payload_and_filing_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());

        // A validly signed v11 payload presented under a v12 filing must be
        // refused: a signature authenticates bytes, not a label.
        let envelope = signed(&v11_fixture_manifest(), TEST_NOW);
        retain_manifest(&paths, &envelope, 12);

        assert!(retained_files(&paths).is_empty());
    }

    #[test]
    fn same_name_different_bytes_is_refused_and_existing_file_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());

        // Pre-place a file at the exact name the v11 payload would use, but with
        // different bytes. Retention must refuse rather than silently replace
        // evidence.
        let envelope = signed(&v11_fixture_manifest(), TEST_NOW);
        let digest = Sha256::digest(envelope.manifest_bytes.as_bytes());
        let digest_hex = format!("{digest:x}");
        let destination = paths
            .manifests_dir
            .join(format!("v11-{}.json", &digest_hex[..16]));
        fs::create_dir_all(&paths.manifests_dir).unwrap();
        let original = b"{\"different\":\"bytes\"}".to_vec();
        fs::write(&destination, &original).unwrap();

        retain_manifest(&paths, &envelope, 11);

        assert_eq!(
            fs::read(&destination).unwrap(),
            original,
            "existing file must be untouched on collision"
        );
    }

    #[cfg(unix)]
    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn retained_manifest_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());

        write_signed_manifest(&paths, v11_fixture_manifest(), TEST_NOW);
        load_manifest(&paths, TEST_NOW).unwrap();

        let files = retained_files(&paths);
        assert_eq!(files.len(), 1);
        let mode = fs::metadata(&files[0]).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    use subc_protocol::{Flags, Frame, FrameType, ModuleHelloAckBody, Priority, PROTOCOL_VERSION};
    use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};

    fn control_flags() -> Flags {
        Flags::new(false, Priority::Passive, false)
    }

    struct SlowDaemonConfig {
        handshake_delay: Duration,
        catalog_delay: Duration,
        open_route_delay: Duration,
        /// When true, the configured stage delays apply only to the first
        /// accepted connection, so a retried probe (attempt 2) sees a fast
        /// daemon. Used to prove the discovery retry succeeds when the first
        /// attempt times out under load.
        first_connection_only: bool,
    }

    struct SlowTestDaemon {
        port: u16,
        key: Vec<u8>,
        daemon_id: [u8; subc_transport::DAEMON_ID_LEN],
        shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
        server_task: Option<std::thread::JoinHandle<()>>,
    }

    impl SlowTestDaemon {
        fn spawn(config: SlowDaemonConfig) -> Self {
            let std_listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("bind test daemon");
            std_listener.set_nonblocking(true).expect("set nonblocking");
            let port = std_listener.local_addr().expect("local addr").port();
            let key = vec![0x42; subc_transport::KEY_LEN];
            let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
            let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

            let key_clone = key.clone();
            let daemon_id_clone = daemon_id;

            let server_task = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                    .expect("build daemon tokio runtime");
                rt.block_on(async move {
                    let listener =
                        tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
                    let mut connection_count = 0usize;
                    loop {
                        tokio::select! {
                            _ = &mut shutdown_rx => break,
                            accepted = listener.accept() => {
                                let Ok((mut stream, _)) = accepted else { break; };
                                let k = key_clone.clone();
                                let d = daemon_id_clone;
                                let first_connection = connection_count == 0;
                                connection_count += 1;
                                let hs_delay = if config.first_connection_only && !first_connection {
                                    Duration::ZERO
                                } else {
                                    config.handshake_delay
                                };
                                let cat_delay = if config.first_connection_only && !first_connection {
                                    Duration::ZERO
                                } else {
                                    config.catalog_delay
                                };
                                let open_delay = if config.first_connection_only && !first_connection {
                                    Duration::ZERO
                                } else {
                                    config.open_route_delay
                                };
                                tokio::spawn(async move {
                                    if hs_delay > Duration::ZERO {
                                        tokio::time::sleep(hs_delay).await;
                                    }
                                    if subc_transport::authenticate_server(
                                        &mut stream,
                                        &k,
                                        &d,
                                        "subc-test",
                                        Duration::from_secs(5),
                                    )
                                    .await
                                    .is_err()
                                    {
                                        return;
                                    }

                                    loop {
                                        let frame = match subc_transport::read_frame(&mut stream).await {
                                            Ok(Some(frame)) => frame,
                                            _ => break,
                                        };

                                        match frame.header.ty {
                                            FrameType::Hello => {
                                                let ack = Frame::build(
                                                    FrameType::HelloAck,
                                                    control_flags(),
                                                    0,
                                                    0,
                                                    frame.header.corr,
                                                    serde_json::to_vec(&ModuleHelloAckBody {
                                                        negotiated_ver: PROTOCOL_VERSION,
                                                        subc_ops: Vec::new(),
                                                        subc_capabilities: Vec::new(),
                                                        storage: None,
                                                        machine_id: None,
                                                    })
                                                    .expect("hello ack body"),
                                                )
                                                .expect("hello ack frame");
                                                if subc_transport::write_frame(&mut stream, &ack).await.is_err() {
                                                    break;
                                                }
                                            }
                                            FrameType::Request => {
                                                let op: Option<String> = serde_json::from_slice::<Value>(&frame.body)
                                                    .ok()
                                                    .and_then(|v| v.get("op").and_then(Value::as_str).map(String::from));

                                                if op.as_deref() == Some("catalog.list") {
                                                    if !cat_delay.is_zero() {
                                                        tokio::time::sleep(cat_delay).await;
                                                    }
                                                    // Governed writes are relayed by the AFT
                                                    // daemon, so discovery looks for `aft`
                                                    // serving the relay operation.
                                                    let response_body = json!({
                                                        "op": "catalog.list",
                                                        "generation": 1,
                                                        "modules": [{
                                                            "module_id": "aft",
                                                            "module_version": "0.1.0",
                                                            "roles": [{
                                                                "role": "management_surface",
                                                                "operations": [{ "name": crate::gh_shim_relay::BOT_REQUEST_OPERATION, "kind": "mutate" }],
                                                                "config_schema": {},
                                                                "observability": [],
                                                                "identity_scope": ["project"]
                                                            }],
                                                            "control_ops": []
                                                        }],
                                                        "subc_ops": ["catalog.list", "route.open"]
                                                    });
                                                    let resp = Frame::build_with_version(
                                                        frame.header.ver,
                                                        FrameType::Response,
                                                        frame.header.flags,
                                                        frame.header.channel,
                                                        frame.header.epoch,
                                                        frame.header.corr,
                                                        serde_json::to_vec(&response_body).expect("catalog json"),
                                                    )
                                                    .expect("catalog response frame");
                                                    if subc_transport::write_frame(&mut stream, &resp).await.is_err() {
                                                        break;
                                                    }
                                                } else if op.as_deref() == Some("route.open") {
                                                    if !open_delay.is_zero() {
                                                        tokio::time::sleep(open_delay).await;
                                                    }
                                                    let response_body = json!({
                                                        "op": "route.open",
                                                        "route_channel": 42,
                                                        "route_epoch": 1
                                                    });
                                                    let resp = Frame::build_with_version(
                                                        frame.header.ver,
                                                        FrameType::Response,
                                                        frame.header.flags,
                                                        frame.header.channel,
                                                        frame.header.epoch,
                                                        frame.header.corr,
                                                        serde_json::to_vec(&response_body).expect("route open json"),
                                                    )
                                                    .expect("route open frame");
                                                    if subc_transport::write_frame(&mut stream, &resp).await.is_err() {
                                                        break;
                                                    }
                                                } else if op.as_deref() == Some("route.close") {
                                                    let response_body = json!({ "op": "route.close" });
                                                    let resp = Frame::build_with_version(
                                                        frame.header.ver,
                                                        FrameType::Response,
                                                        frame.header.flags,
                                                        frame.header.channel,
                                                        frame.header.epoch,
                                                        frame.header.corr,
                                                        serde_json::to_vec(&response_body).expect("route close json"),
                                                    )
                                                    .expect("route close frame");
                                                    if subc_transport::write_frame(&mut stream, &resp).await.is_err() {
                                                        break;
                                                    }
                                                } else if frame.header.channel == 42 {
                                                    let response_body = json!({
                                                        "outcome": "result",
                                                        "gh_route_schema": 1,
                                                        "result": { "url": "https://github.com/cortexkit/aft/issues/1#issuecomment-123" },
                                                        "field_order": ["url"]
                                                    });
                                                    let resp = Frame::build_with_version(
                                                        frame.header.ver,
                                                        FrameType::Response,
                                                        frame.header.flags,
                                                        frame.header.channel,
                                                        frame.header.epoch,
                                                        frame.header.corr,
                                                        serde_json::to_vec(&response_body).expect("result json"),
                                                    )
                                                    .expect("result frame");
                                                    if subc_transport::write_frame(&mut stream, &resp).await.is_err() {
                                                        break;
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                });
                            }
                        }
                    }
                });
            });

            Self {
                port,
                key,
                daemon_id,
                shutdown_tx: Some(shutdown_tx),
                server_task: Some(server_task),
            }
        }

        fn write_connection_file(&self, path: &Path) {
            let conn = ConnectionInfo {
                schema: SCHEMA_VERSION,
                wire_version: Some(PROTOCOL_VERSION),
                endpoints: vec![Endpoint {
                    host: "127.0.0.1".to_string(),
                    port: self.port,
                }],
                key: self.key.clone(),
                daemon_id: self.daemon_id,
                pid: std::process::id(),
                daemon_ver: "gh-shim-test-daemon".to_string(),
            };
            connection_file::write_atomic(path, &conn).expect("write test daemon connection file");
        }
    }

    impl Drop for SlowTestDaemon {
        fn drop(&mut self) {
            if let Some(tx) = self.shutdown_tx.take() {
                let _ = tx.send(());
            }
            let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
            if let Some(task) = self.server_task.take() {
                let _ = task.join();
            }
        }
    }

    fn write_test_project_repo(root: &Path, repository: &str) -> PathBuf {
        let project = root.join("test-project");
        fs::create_dir_all(&project).expect("create project directory");
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&project)
            .status()
            .expect("init git repo");
        Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                &format!("https://github.com/{repository}.git"),
            ])
            .current_dir(&project)
            .status()
            .expect("add git origin");
        project
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cached_lower_rungs_are_reused_only_with_same_repository_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(temp.path().join("state"));
        let connection = temp.path().join("connection.json");
        fs::write(&connection, "{}").unwrap();
        let doc = json!({ "subc": { "connection_file": connection } }).to_string();
        let project = write_test_project_repo(temp.path(), "cortexkit/motor");
        write_signed_manifest(&paths, v12_fixture_manifest(), TEST_NOW);
        let provenance = RungRecordProvenance::for_target(&TargetRepository::default(), &project);

        for rung in [Rung::R1, Rung::R2] {
            for repo in [
                Some("cortexkit/motor"),
                Some("cortexkit/aft"),
                Some("unresolved (no GitHub origin)"),
                None,
            ] {
                let mut record = RungDetermination::r2(
                    TEST_NOW - 1,
                    R2Reason::AgentBindingUnavailable,
                    Some(12),
                    &provenance,
                )
                .record;
                record.rung = rung;
                record.recorded_by_repo_key = repo.map(str::to_string);
                write_rung_record_silently(&paths, &record);
                let determination = determine_rung_from_doc(
                    &paths,
                    &project,
                    TEST_NOW,
                    Instant::now() + DISCOVERY_BUDGET,
                    Some(&doc),
                );
                let same_repo = repo == Some("cortexkit/motor");
                assert_eq!(
                    determination.record.as_of_unix_secs,
                    if same_repo { TEST_NOW - 1 } else { TEST_NOW },
                    "cached {rung:?} provenance {repo:?}"
                );
                assert_eq!(
                    determination.record.rung,
                    if same_repo { rung } else { Rung::R2 }
                );
                assert_eq!(
                    determination.record.recorded_by_repo_key.as_deref(),
                    Some("cortexkit/motor")
                );
            }
        }
    }

    #[test]
    fn cached_rung_deadline_fallback_requires_same_repository_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(temp.path().join("state"));
        let connection = temp.path().join("connection.json");
        fs::write(&connection, "{}").unwrap();
        let doc = json!({ "subc": { "connection_file": connection } }).to_string();
        let project = write_test_project_repo(temp.path(), "cortexkit/aft");

        for (rung, age) in [(Rung::R2, 1), (Rung::R3, 1), (Rung::R3, 30)] {
            for repo in [Some("cortexkit/aft"), Some("cortexkit/motor"), None] {
                let mut record =
                    RungDetermination::r3(TEST_NOW - age, 12, &test_rung_provenance()).record;
                record.rung = rung;
                record.recorded_by_repo_key = repo.map(str::to_string);
                write_rung_record_silently(&paths, &record);
                let determination =
                    determine_rung_from_doc(&paths, &project, TEST_NOW, Instant::now(), Some(&doc));
                assert_eq!(
                    determination.record.rung,
                    if repo == Some("cortexkit/aft") {
                        rung
                    } else {
                        Rung::R1
                    },
                    "deadline fallback for {rung:?} provenance {repo:?}"
                );
            }
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cached_unbound_repository_does_not_poison_bound_governed_write() {
        let daemon = SlowTestDaemon::spawn(SlowDaemonConfig {
            handshake_delay: Duration::ZERO,
            catalog_delay: Duration::ZERO,
            open_route_delay: Duration::ZERO,
            first_connection_only: false,
        });
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(temp.path().join("state"));
        let connection = temp.path().join("connection.json");
        daemon.write_connection_file(&connection);
        let doc = json!({ "subc": { "connection_file": connection } }).to_string();
        let unbound = write_test_project_repo(&temp.path().join("unbound"), "cortexkit/motor");
        let bound = write_test_project_repo(&temp.path().join("bound"), "cortexkit/aft");
        let manifest = v12_fixture_manifest();
        let now = TEST_NOW;
        write_signed_manifest(&paths, manifest.clone(), now);

        let unbound_determination = determine_rung_from_doc(
            &paths,
            &unbound,
            now,
            Instant::now() + DISCOVERY_BUDGET,
            Some(&doc),
        );
        assert_eq!(unbound_determination.record.rung, Rung::R2);
        assert_eq!(
            unbound_determination.record.inputs["agent_binding_unavailable"],
            "failed"
        );
        assert_eq!(
            load_rung_record(&paths)
                .unwrap()
                .recorded_by_repo_key
                .as_deref(),
            Some("cortexkit/motor")
        );
        assert!(read_last_probe(&paths).is_none());

        let args = os_args(&["issue", "comment", "1", "--body", "bound bot comment"]);
        let target = TargetRepository::default();
        let determination = determine_rung_for_target(
            &paths,
            &target,
            &bound,
            now + 1,
            Instant::now() + DISCOVERY_BUDGET,
            Some(&doc),
        );
        assert!(
            matches!(
                non_r3_governance_disposition(
                    &bound,
                    &target,
                    &determination,
                    &args,
                    &manifest,
                    current_platform(),
                ),
                GovernanceDisposition::Ready
            ),
            "a bound governed write must rediscover instead of reusing the unbound repo's R2: {:?}",
            determination.record
        );
        assert_eq!(determination.record.rung, Rung::R3);
        assert_eq!(
            load_rung_record(&paths)
                .unwrap()
                .recorded_by_repo_key
                .as_deref(),
            Some("cortexkit/aft")
        );
        assert_eq!(read_last_probe(&paths).unwrap().outcome, "ready");
    }

    /// Stage-naming tests: a deadline wide enough that a listening loopback daemon's
    /// connect and handshake finish under it even on a loaded Windows runner (train 51:
    /// the old 150 ms budget was blown at the connect stage, so the injected
    /// catalog_list delay was never reached and the assertion read the connect-stage
    /// outcome), with the injected stage delay far beyond it so the named stage is the
    /// one that times out. The discovery budget is now 2 s per stage, so the injected
    /// delay must exceed 2 s to force a timeout.
    const STAGE_TEST_DEADLINE: Duration = Duration::from_secs(2);

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn probe_exceeded_at_catalog_list_names_stage_and_budget_in_status_and_refusal() {
        let daemon = SlowTestDaemon::spawn(SlowDaemonConfig {
            handshake_delay: Duration::ZERO,
            catalog_delay: Duration::from_secs(6),
            open_route_delay: Duration::ZERO,
            first_connection_only: false,
        });
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(temp.path().join("state"));
        let conn_file = temp.path().join("subc-connection.json");
        daemon.write_connection_file(&conn_file);
        let project = write_test_project_repo(temp.path(), "cortexkit/aft");

        let manifest = v12_fixture_manifest();
        let now = unix_seconds();
        write_signed_manifest(&paths, manifest, now);

        let doc = json!({
            "subc": { "connection_file": conn_file.to_str().unwrap() }
        })
        .to_string();

        let determination = determine_rung_from_doc(
            &paths,
            &project,
            now,
            Instant::now() + STAGE_TEST_DEADLINE,
            Some(&doc),
        );
        assert_eq!(determination.record.rung, Rung::R1);
        assert_eq!(
            determination.refusal_detail.as_deref(),
            Some(
                "governance probe timed out after 2000 ms at catalog_list (daemon may be busy; host load?) - this repository's actions are identity-governed, so the command was not run; retry"
            )
        );

        let last_probe = read_last_probe(&paths).expect("last probe record");
        assert_eq!(last_probe.stage, "catalog_list");
        assert_eq!(last_probe.outcome, "timed_out");
        assert!(last_probe.elapsed_ms >= 2000);

        let report = render_self_report(&paths).expect("self report");
        let status: Value = serde_json::from_str(&report).expect("status json");
        assert_eq!(status["last_probe"]["stage"], "catalog_list");
        assert_eq!(status["last_probe"]["outcome"], "timed_out");
        assert!(status["last_probe"]["elapsed_ms"].as_u64().unwrap() >= 2000);
    }

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn probe_exceeded_at_open_route_names_stage_and_budget_in_status_and_refusal() {
        let daemon = SlowTestDaemon::spawn(SlowDaemonConfig {
            handshake_delay: Duration::ZERO,
            catalog_delay: Duration::ZERO,
            open_route_delay: Duration::from_secs(6),
            first_connection_only: false,
        });
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(temp.path().join("state"));
        let conn_file = temp.path().join("subc-connection.json");
        daemon.write_connection_file(&conn_file);
        let project = write_test_project_repo(temp.path(), "cortexkit/aft");

        let manifest = v12_fixture_manifest();
        let now = unix_seconds();
        write_signed_manifest(&paths, manifest, now);

        let doc = json!({
            "subc": { "connection_file": conn_file.to_str().unwrap() }
        })
        .to_string();

        let determination = determine_rung_from_doc(
            &paths,
            &project,
            now,
            Instant::now() + STAGE_TEST_DEADLINE,
            Some(&doc),
        );
        assert_eq!(determination.record.rung, Rung::R1);
        assert_eq!(
            determination.refusal_detail.as_deref(),
            Some("governance probe timed out after 2000 ms at open_route (daemon may be busy; host load?) - this repository's actions are identity-governed, so the command was not run; retry")
        );

        let last_probe = read_last_probe(&paths).expect("last probe record");
        assert_eq!(last_probe.stage, "open_route");
        assert_eq!(last_probe.outcome, "timed_out");
        assert!(last_probe.elapsed_ms >= 2000);

        let report = render_self_report(&paths).expect("self report");
        let status: Value = serde_json::from_str(&report).expect("status json");
        assert_eq!(status["last_probe"]["stage"], "open_route");
        assert_eq!(status["last_probe"]["outcome"], "timed_out");
        assert!(status["last_probe"]["elapsed_ms"].as_u64().unwrap() >= 2000);
    }

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn discovery_retry_succeeds_when_only_the_first_attempt_times_out() {
        // The daemon delays only the first accepted connection, so the first
        // probe attempt times out at the catalog_list stage and the single
        // retry (after the 250 ms backoff) sees a fast daemon and reaches R3.
        let daemon = SlowTestDaemon::spawn(SlowDaemonConfig {
            handshake_delay: Duration::ZERO,
            catalog_delay: Duration::from_secs(6),
            open_route_delay: Duration::ZERO,
            first_connection_only: true,
        });
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(temp.path().join("state"));
        let conn_file = temp.path().join("subc-connection.json");
        daemon.write_connection_file(&conn_file);
        let project = write_test_project_repo(temp.path(), "cortexkit/aft");

        let manifest = v12_fixture_manifest();
        let now = unix_seconds();
        write_signed_manifest(&paths, manifest, now);

        let doc = json!({
            "subc": { "connection_file": conn_file.to_str().unwrap() }
        })
        .to_string();

        let determination = determine_rung_from_doc(
            &paths,
            &project,
            now,
            Instant::now() + STAGE_TEST_DEADLINE,
            Some(&doc),
        );
        assert_eq!(determination.record.rung, Rung::R3);
        assert!(determination.refusal_detail.is_none());

        // The retry succeeded, so the last probe records the successful attempt.
        let last_probe = read_last_probe(&paths).expect("last probe record");
        assert_eq!(last_probe.outcome, "ready");
    }

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn probe_connect_refused_keeps_unreachable_outcome() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let port = listener.local_addr().expect("port").port();
        drop(listener);

        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(temp.path().join("state"));
        let conn_file = temp.path().join("subc-connection.json");
        let conn = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".to_string(),
                port,
            }],
            key: vec![0x42; subc_transport::KEY_LEN],
            daemon_id: [0x24; subc_transport::DAEMON_ID_LEN],
            pid: std::process::id(),
            daemon_ver: "dead".to_string(),
        };
        connection_file::write_atomic(&conn_file, &conn).expect("write connection file");
        let project = write_test_project_repo(temp.path(), "cortexkit/aft");

        let manifest = v12_fixture_manifest();
        let now = unix_seconds();
        write_signed_manifest(&paths, manifest, now);

        let doc = json!({
            "subc": { "connection_file": conn_file.to_str().unwrap() }
        })
        .to_string();

        let determination = determine_rung_from_doc(
            &paths,
            &project,
            now,
            Instant::now() + DISCOVERY_BUDGET,
            Some(&doc),
        );
        assert_eq!(determination.record.rung, Rung::R2);
        assert_eq!(determination.refusal_detail, None);

        let last_probe = read_last_probe(&paths).expect("last probe record");
        assert_eq!(last_probe.stage, "connect");
        #[cfg(windows)]
        assert!(
            last_probe.outcome == "timed_out" || last_probe.outcome == "unreachable",
            "windows connect-refused probe outcome was {}",
            last_probe.outcome
        );
        #[cfg(not(windows))]
        assert_eq!(last_probe.outcome, "unreachable");

        let report = render_self_report(&paths).expect("self report");
        let status: Value = serde_json::from_str(&report).expect("status json");
        assert_eq!(status["last_probe"]["stage"], "connect");
        #[cfg(windows)]
        assert!(
            status["last_probe"]["outcome"] == "timed_out"
                || status["last_probe"]["outcome"] == "unreachable"
        );
        #[cfg(not(windows))]
        assert_eq!(status["last_probe"]["outcome"], "unreachable");
    }

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn probe_connect_stage_budget_exceeded_determines_r2_and_records_last_probe() {
        let daemon = SlowTestDaemon::spawn(SlowDaemonConfig {
            handshake_delay: Duration::from_secs(6),
            catalog_delay: Duration::ZERO,
            open_route_delay: Duration::ZERO,
            first_connection_only: false,
        });
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(temp.path().join("state"));
        let conn_file = temp.path().join("subc-connection.json");
        daemon.write_connection_file(&conn_file);
        let project = write_test_project_repo(temp.path(), "cortexkit/aft");

        let manifest = v12_fixture_manifest();
        let now = unix_seconds();
        write_signed_manifest(&paths, manifest, now);

        let doc = json!({
            "subc": { "connection_file": conn_file.to_str().unwrap() }
        })
        .to_string();

        let determination = determine_rung_from_doc(
            &paths,
            &project,
            now,
            Instant::now() + DISCOVERY_BUDGET,
            Some(&doc),
        );
        assert_eq!(determination.record.rung, Rung::R2);
        assert_eq!(determination.refusal_detail, None);

        let last_probe = read_last_probe(&paths).expect("last probe record");
        assert_eq!(last_probe.stage, "connect");
        assert_eq!(last_probe.outcome, "timed_out");
        assert!(last_probe.elapsed_ms >= 2000);

        let report = render_self_report(&paths).expect("self report");
        let status: Value = serde_json::from_str(&report).expect("status json");
        assert_eq!(status["last_probe"]["stage"], "connect");
        assert_eq!(status["last_probe"]["outcome"], "timed_out");
        assert!(status["last_probe"]["elapsed_ms"].as_u64().unwrap() >= 2000);
    }

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn slow_daemon_connect_delay_fallback_active_determines_r3_and_records_last_probe() {
        let daemon = SlowTestDaemon::spawn(SlowDaemonConfig {
            handshake_delay: Duration::from_secs(6),
            catalog_delay: Duration::ZERO,
            open_route_delay: Duration::ZERO,
            first_connection_only: false,
        });
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(temp.path().join("state"));
        let conn_file = temp.path().join("subc-connection.json");
        daemon.write_connection_file(&conn_file);
        let project = write_test_project_repo(temp.path(), "cortexkit/aft");

        let manifest = v12_fixture_manifest();
        let now = unix_seconds();
        write_signed_manifest(&paths, manifest, now);

        let mut cached_record = RungDetermination::r3(now - 30, 12, &test_rung_provenance()).record;
        cached_record.last_reachable_unix_secs = Some(now - 30);
        write_rung_record_silently(&paths, &cached_record);

        let doc = json!({
            "subc": { "connection_file": conn_file.to_str().unwrap() }
        })
        .to_string();

        let determination = determine_rung_from_doc(
            &paths,
            &project,
            now,
            Instant::now() + DISCOVERY_BUDGET,
            Some(&doc),
        );
        assert_eq!(determination.record.rung, Rung::R3);
        assert!(determination.refusal_detail.is_none());

        let last_probe = read_last_probe(&paths).expect("last probe record");
        assert_eq!(last_probe.stage, "connect");
        assert_eq!(last_probe.outcome, "timed_out");
        assert!(last_probe.elapsed_ms >= 2000);

        let report = render_self_report(&paths).expect("self report");
        let status: Value = serde_json::from_str(&report).expect("status json");
        assert_eq!(status["last_probe"]["stage"], "connect");
        assert_eq!(status["last_probe"]["outcome"], "timed_out");
        assert!(status["last_probe"]["elapsed_ms"].as_u64().unwrap() >= 2000);
    }

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn slow_daemon_catalog_list_delay_fallback_active_determines_r3_and_records_last_probe() {
        let daemon = SlowTestDaemon::spawn(SlowDaemonConfig {
            handshake_delay: Duration::ZERO,
            catalog_delay: Duration::from_secs(6),
            open_route_delay: Duration::ZERO,
            first_connection_only: false,
        });
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(temp.path().join("state"));
        let conn_file = temp.path().join("subc-connection.json");
        daemon.write_connection_file(&conn_file);
        let project = write_test_project_repo(temp.path(), "cortexkit/aft");

        let manifest = v12_fixture_manifest();
        let now = unix_seconds();
        write_signed_manifest(&paths, manifest, now);

        let mut cached_record = RungDetermination::r3(now - 30, 12, &test_rung_provenance()).record;
        cached_record.last_reachable_unix_secs = Some(now - 30);
        write_rung_record_silently(&paths, &cached_record);

        let doc = json!({
            "subc": { "connection_file": conn_file.to_str().unwrap() }
        })
        .to_string();

        let determination = determine_rung_from_doc(
            &paths,
            &project,
            now,
            Instant::now() + DISCOVERY_BUDGET,
            Some(&doc),
        );
        assert_eq!(determination.record.rung, Rung::R3);
        assert!(determination.refusal_detail.is_none());

        let last_probe = read_last_probe(&paths).expect("last probe record");
        assert_eq!(last_probe.stage, "catalog_list");
        assert_eq!(last_probe.outcome, "timed_out");
        assert!(last_probe.elapsed_ms >= 2000);

        let report = render_self_report(&paths).expect("self report");
        let status: Value = serde_json::from_str(&report).expect("status json");
        assert_eq!(status["last_probe"]["stage"], "catalog_list");
        assert_eq!(status["last_probe"]["outcome"], "timed_out");
        assert!(status["last_probe"]["elapsed_ms"].as_u64().unwrap() >= 2000);
    }

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn slow_daemon_catalog_list_delay_expired_fallback_refuses_naming_stage() {
        let daemon = SlowTestDaemon::spawn(SlowDaemonConfig {
            handshake_delay: Duration::ZERO,
            catalog_delay: Duration::from_secs(6),
            open_route_delay: Duration::ZERO,
            first_connection_only: false,
        });
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(temp.path().join("state"));
        let conn_file = temp.path().join("subc-connection.json");
        daemon.write_connection_file(&conn_file);
        let project = write_test_project_repo(temp.path(), "cortexkit/aft");

        let manifest = v12_fixture_manifest();
        let now = unix_seconds();
        write_signed_manifest(&paths, manifest, now);

        let mut cached_record =
            RungDetermination::r3(now - 301, 12, &test_rung_provenance()).record;
        cached_record.last_reachable_unix_secs = Some(now - 301);
        write_rung_record_silently(&paths, &cached_record);

        let doc = json!({
            "subc": { "connection_file": conn_file.to_str().unwrap() }
        })
        .to_string();

        let determination = determine_rung_from_doc(
            &paths,
            &project,
            now,
            Instant::now() + DISCOVERY_BUDGET,
            Some(&doc),
        );
        assert_eq!(determination.record.rung, Rung::R1);
        assert_eq!(
            determination.refusal_detail.as_deref(),
            Some(
                "governance probe timed out after 2000 ms at catalog_list (daemon may be busy; host load?) - this repository's actions are identity-governed, so the command was not run; retry"
            )
        );

        let report = render_self_report(&paths).expect("self report");
        let status: Value = serde_json::from_str(&report).expect("status json");
        assert_eq!(status["last_probe"]["stage"], "catalog_list");
        assert_eq!(status["last_probe"]["outcome"], "timed_out");
        assert!(status["last_probe"]["elapsed_ms"].as_u64().unwrap() >= 2000);
    }

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn slow_daemon_open_route_delay_fallback_active_determines_r3_and_records_last_probe() {
        let daemon = SlowTestDaemon::spawn(SlowDaemonConfig {
            handshake_delay: Duration::ZERO,
            catalog_delay: Duration::ZERO,
            open_route_delay: Duration::from_secs(6),
            first_connection_only: false,
        });
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(temp.path().join("state"));
        let conn_file = temp.path().join("subc-connection.json");
        daemon.write_connection_file(&conn_file);
        let project = write_test_project_repo(temp.path(), "cortexkit/aft");

        let manifest = v12_fixture_manifest();
        let now = unix_seconds();
        write_signed_manifest(&paths, manifest, now);

        let mut cached_record = RungDetermination::r3(now - 30, 12, &test_rung_provenance()).record;
        cached_record.last_reachable_unix_secs = Some(now - 30);
        write_rung_record_silently(&paths, &cached_record);

        let doc = json!({
            "subc": { "connection_file": conn_file.to_str().unwrap() }
        })
        .to_string();

        let determination = determine_rung_from_doc(
            &paths,
            &project,
            now,
            Instant::now() + DISCOVERY_BUDGET,
            Some(&doc),
        );
        assert_eq!(determination.record.rung, Rung::R3);
        assert!(determination.refusal_detail.is_none());

        let last_probe = read_last_probe(&paths).expect("last probe record");
        assert_eq!(last_probe.stage, "open_route");
        assert_eq!(last_probe.outcome, "timed_out");
        assert!(last_probe.elapsed_ms >= 2000);

        let report = render_self_report(&paths).expect("self report");
        let status: Value = serde_json::from_str(&report).expect("status json");
        assert_eq!(status["last_probe"]["stage"], "open_route");
        assert_eq!(status["last_probe"]["outcome"], "timed_out");
        assert!(status["last_probe"]["elapsed_ms"].as_u64().unwrap() >= 2000);
    }

    #[cfg(debug_assertions)] // verifies under the dev test key, which release trust sets exclude
    #[test]
    fn slow_daemon_open_route_delay_expired_fallback_refuses_naming_stage() {
        let daemon = SlowTestDaemon::spawn(SlowDaemonConfig {
            handshake_delay: Duration::ZERO,
            catalog_delay: Duration::ZERO,
            open_route_delay: Duration::from_secs(6),
            first_connection_only: false,
        });
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(temp.path().join("state"));
        let conn_file = temp.path().join("subc-connection.json");
        daemon.write_connection_file(&conn_file);
        let project = write_test_project_repo(temp.path(), "cortexkit/aft");

        let manifest = v12_fixture_manifest();
        let now = unix_seconds();
        write_signed_manifest(&paths, manifest, now);

        let mut cached_record =
            RungDetermination::r3(now - 301, 12, &test_rung_provenance()).record;
        cached_record.last_reachable_unix_secs = Some(now - 301);
        write_rung_record_silently(&paths, &cached_record);

        let doc = json!({
            "subc": { "connection_file": conn_file.to_str().unwrap() }
        })
        .to_string();

        let determination = determine_rung_from_doc(
            &paths,
            &project,
            now,
            Instant::now() + DISCOVERY_BUDGET,
            Some(&doc),
        );
        assert_eq!(determination.record.rung, Rung::R1);
        assert_eq!(
            determination.refusal_detail.as_deref(),
            Some("governance probe timed out after 2000 ms at open_route (daemon may be busy; host load?) - this repository's actions are identity-governed, so the command was not run; retry")
        );

        let report = render_self_report(&paths).expect("self report");
        let status: Value = serde_json::from_str(&report).expect("status json");
        assert_eq!(status["last_probe"]["stage"], "open_route");
        assert_eq!(status["last_probe"]["outcome"], "timed_out");
        assert!(status["last_probe"]["elapsed_ms"].as_u64().unwrap() >= 2000);
    }
}

#[cfg(test)]
thread_local! {
    static MANIFEST_WORK: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
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
            author_scope: None,
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
    fn stderr_state_confirmation_invalidates_the_changed_thread() {
        let storage = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&storage.path().join("aft.db")).unwrap();
        write_cached_issue(&conn, "cortexkit/aft", 42, "principal:alice");
        let request = github_read_mutation_request("issue close", "cortexkit/aft", 42);
        let mutation = GithubReadMutation::from_governed_request(&request).unwrap();
        invalidate_successful_github_read_mutation_at(
            storage.path(),
            Some(&mutation),
            &RouteOutcome::ResultStderr("✓ Closed issue cortexkit/aft#42\n".into()),
        );
        assert!(!cached_issue_exists(
            &conn,
            "cortexkit/aft",
            42,
            "principal:alice"
        ));
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
    fn issue_edit_maps_to_the_edited_issue_cache_resource() {
        let request = github_read_mutation_request("issue edit", "CortexKit/AFT", 42);
        let mutation = GithubReadMutation::from_governed_request(&request)
            .expect("structured issue edit has a cache resource");
        assert_eq!(mutation.normalized_repository, "cortexkit/aft");
        assert_eq!(mutation.resource_kind, GithubReadResourceKind::Issue);
        assert_eq!(mutation.resource_number, 42);
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
