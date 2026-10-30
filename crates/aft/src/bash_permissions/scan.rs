use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tree_sitter::{Node, Parser, Tree};

use crate::context::AppContext;

use super::{arity, PermissionAsk, PermissionKind};

const FILE_COMMANDS: &[&str] = &[
    "rm", "cp", "mv", "mkdir", "touch", "chmod", "chown", "cat", "cd", "source", ".",
];
const CWD_COMMANDS: &[&str] = &["cd", "pushd", "popd"];
pub const UNKNOWN_CWD_PERMISSION_REASON: &str =
    "Working directory is unknown; approve this exact command.";
/// File commands whose path arguments are only read. Only these (and `<`
/// input redirects) may use a read exemption; a command that can modify,
/// move or delete its arguments always gets the normal external-directory ask.
const READ_ONLY_FILE_COMMANDS: &[&str] = &["cat"];

/// Decides whether reading one resolved out-of-root path needs no
/// external-directory ask. The hoisted bash tool passes the background-task
/// registry's exact, session-scoped artifact check here, so a session can read
/// its own task output files (which live under the AFT storage root, outside
/// every project) without a prompt whose "always allow" could never match the
/// next task's freshly named directory.
type ReadExemption<'a> = &'a dyn Fn(&Path) -> bool;

#[derive(Debug, Clone)]
struct Part {
    text: String,
}

pub fn scan(command: &str, ctx: &AppContext) -> Vec<PermissionAsk> {
    #[cfg(windows)]
    {
        let _ = (command, ctx);
        Vec::new()
    }

    #[cfg(not(windows))]
    {
        let config = ctx.config();
        if !config.bash_permissions {
            return Vec::new();
        }
        let Some(project_root) = config.project_root.clone() else {
            return Vec::new();
        };
        drop(config);

        scan_with_cwd(command, ctx, &project_root)
    }
}

pub fn scan_with_cwd(command: &str, ctx: &AppContext, cwd: &Path) -> Vec<PermissionAsk> {
    let Some(project_root) = ctx.config().project_root.clone() else {
        return Vec::new();
    };
    scan_with_project_root(command, &project_root, cwd)
}

/// Scan like [`scan_with_cwd`], but let `session_id` read the stdout, stderr,
/// exit and pty files of its own background bash tasks without an
/// external-directory ask.
///
/// Ownership is decided by
/// [`crate::bash_background::BgTaskRegistry::is_session_owned_artifact_path`],
/// the same exact check `AppContext::validate_read_path` uses: the canonical
/// path must equal a registered artifact of a task owned by this session.
/// There is no `bash-tasks` prefix rule, so other sessions' artifacts and
/// unregistered files next to an artifact still ask. The exemption covers
/// only read-only uses (`cat` arguments and `<` input redirects); every other
/// out-of-root path in the command still produces its own ask.
pub fn scan_with_cwd_for_session(
    command: &str,
    ctx: &AppContext,
    cwd: &Path,
    session_id: &str,
) -> Vec<PermissionAsk> {
    let Some(project_root) = ctx.config().project_root.clone() else {
        return Vec::new();
    };
    let registry = ctx.bash_background();
    let owned_artifact = |path: &Path| registry.is_session_owned_artifact_path(session_id, path);
    scan_with_read_exemption(command, &project_root, cwd, Some(&owned_artifact))
}

pub fn scan_with_project_root(
    command: &str,
    project_root: &Path,
    cwd: &Path,
) -> Vec<PermissionAsk> {
    scan_with_read_exemption(command, project_root, cwd, None)
}

fn scan_with_read_exemption(
    command: &str,
    project_root: &Path,
    cwd: &Path,
    read_exemption: Option<ReadExemption<'_>>,
) -> Vec<PermissionAsk> {
    let project_root = resolve_existing(&project_root);
    let cwd = resolve_existing(cwd);

    let Some(tree) = parse_tree(command) else {
        // Permission scanning fails closed when syntax cannot be understood.
        return vec![parse_failed_ask()];
    };

    let root = tree.root_node();
    let mut command_nodes = Vec::new();
    collect_commands(root, &mut command_nodes);

    // Fail-closed: tree-sitter parsed successfully but found NO `command`
    // nodes, yet the input has visible content. This happens for shapes
    // that have side effects without invoking a command word, e.g.:
    //   - pure redirects:  `> /tmp/x` (truncate file)
    //   - variable-only:   `FOO=bar`  (set shell variable)
    //   - empty subshell:  `()`
    // Without this fall-back the `bash.rs` guard
    // `if !permission_asks.is_empty()` skips the permission check
    // entirely and the user's `bash: { "*": deny }` rule is bypassed.
    //
    // Pure `cd` does NOT trigger this branch because tree-sitter still
    // produces a `command` node for it; the cd handling above just
    // chooses not to push an ask for that node, which matches OpenCode
    // and is intentional (cd has no externally visible effect beyond
    // updating scan_cwd, already handled).
    if command_nodes.is_empty() && !command.trim().is_empty() {
        return vec![parse_failed_ask()];
    }

    let mut asks = Vec::new();
    let mut seen = HashSet::new();
    // Unknown directory changes must not resolve later paths against a stale base.
    let mut scan_cwd = Some(cwd.clone());
    let mut unknown_cwd = false;
    // Directories pushed by `pushd` earlier in this command line. The shell's
    // own stack from before the command is unknown, so a `popd` past these
    // entries leaves the directory unknown.
    let mut directory_stack = Vec::new();
    for node in command_nodes {
        let parts = command_parts(command, node);
        if parts.is_empty() {
            continue;
        }

        let tokens: Vec<String> = parts.iter().map(|part| part.text.clone()).collect();
        let head = tokens[0].as_str();

        if CWD_COMMANDS.contains(&head) {
            collect_redirection_targets(
                command,
                node,
                scan_cwd.as_deref(),
                |target| match target {
                    RedirectTarget::Path { path, input_only } => {
                        let exemption = if input_only { read_exemption } else { None };
                        push_external_path(&mut asks, &mut seen, &project_root, &path, exemption);
                    }
                    RedirectTarget::RelativeToUnknown => {
                        unknown_cwd = true;
                    }
                    RedirectTarget::Dynamic => {}
                },
            );
            let home = std::env::var_os("HOME").map(PathBuf::from);
            track_cwd_change(
                head,
                &parts,
                home.as_deref(),
                &mut scan_cwd,
                &mut directory_stack,
            );
            continue;
        }

        unknown_cwd |= scan_cwd.is_none();
        if FILE_COMMANDS.contains(&head) {
            let exemption = if READ_ONLY_FILE_COMMANDS.contains(&head) {
                read_exemption
            } else {
                None
            };
            for arg in path_args(&parts) {
                match path_arg_target(arg, scan_cwd.as_deref()) {
                    PathArgTarget::Path(path) => {
                        push_external_path(&mut asks, &mut seen, &project_root, &path, exemption);
                    }
                    PathArgTarget::RelativeToUnknown => {
                        unknown_cwd = true;
                    }
                    // A dynamic target (e.g. `rm "$DEST/file"`) can't be
                    // resolved to a concrete directory. Matching native
                    // OpenCode — which skips dynamic path args entirely — we
                    // emit NO external_directory ask: the bash command approval
                    // below already gates the command, and the old wildcard ask
                    // (`patterns/always: ["*"]`) both rendered as a confusing
                    // "Access external directory ." and, if "always"-approved,
                    // granted blanket external-directory access. See issue #135.
                    PathArgTarget::Dynamic | PathArgTarget::None => {}
                }
            }
        }

        collect_redirection_targets(command, node, scan_cwd.as_deref(), |target| match target {
            RedirectTarget::Path { path, input_only } => {
                // Only a plain `<` input redirect reads its target; `>`, `>>`,
                // `&>`, `<>` and friends can create or truncate it.
                let exemption = if input_only { read_exemption } else { None };
                push_external_path(&mut asks, &mut seen, &project_root, &path, exemption);
            }
            RedirectTarget::RelativeToUnknown => {
                unknown_cwd = true;
            }
            // Dynamic redirect target (e.g. `echo hi > "$FOO/bar"`): no
            // external_directory ask, same rationale as dynamic path args
            // above — match native OpenCode and rely on the bash approval.
            // See issue #135.
            RedirectTarget::Dynamic => {}
        });

        // Mirror OpenCode's `packages/opencode/src/tool/bash.ts`: only
        // skip `cd`/`pushd`/`popd` (they have no externally visible effect
        // beyond updating scan_cwd, already handled above). Every other
        // command — including `echo` — must produce a bash ask so the
        // user's permission rules (`bash: { "*": deny, ... }`) actually
        // apply. Previous code excluded `echo` here, which let any
        // command starting with `echo` bypass deny rules.
        if !CWD_COMMANDS.contains(&head) {
            push_bash_ask(&mut asks, &mut seen, source(command, node), &tokens);
            if head == "xargs" {
                push_xargs_ask(&mut asks, &mut seen, &tokens);
            }
        }
    }

    if unknown_cwd {
        // A relative folder approval has no stable identity after a dynamic cd.
        // Add exact-command approval without replacing the individual command
        // resources that the host checks against its allow and deny rules.
        asks.push(PermissionAsk {
            kind: PermissionKind::Bash,
            patterns: vec![command.to_string()],
            always: Vec::new(),
            reason: Some(UNKNOWN_CWD_PERMISSION_REASON.into()),
            targets: Vec::new(),
        });
    }
    asks
}

fn parse_failed_ask() -> PermissionAsk {
    PermissionAsk {
        kind: PermissionKind::Bash,
        patterns: vec!["*".to_string()],
        always: Vec::new(),
        reason: None,
        targets: Vec::new(),
    }
}

/// The permission scanner and worker load guard share the same bash grammar
/// and definition of a successful parse, but choose their own failure policy.
pub(super) fn parse_tree(command: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(command, None)?;
    (!tree.root_node().has_error()).then_some(tree)
}

pub(super) fn collect_commands<'tree>(node: Node<'tree>, out: &mut Vec<Node<'tree>>) {
    if node.kind() == "command" {
        out.push(node);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_commands(child, out);
    }
}

#[cfg(any(unix, test))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DirectoryTarget {
    pub path: PathBuf,
    /// An uncertain shell mode requires both candidate destinations to be invalid.
    pub alternate_path: Option<PathBuf>,
    pub literal_absolute: bool,
}

/// This runon check is best-effort: callers refuse only changes known to be
/// invalid. Shell behavior the check cannot follow continues to run as before.
/// Follow reachable outer-shell folder and physical-mode alternatives, keeping
/// successful and failed commands separate so `&&` and `||` stay correlated.
/// `known_directory` identifies existing directories copied to the runner. Treat
/// valid initial `cd` changes as successful, but stop trusting the snapshot after
/// any non-`cd` command. Keep tracking cwd independently of snapshot trust.
#[cfg(any(unix, test))]
pub(crate) fn top_level_directory_targets(
    command: &str,
    cwd: &Path,
    known_directory: &dyn Fn(&Path) -> bool,
) -> Vec<DirectoryTarget> {
    const MAX_CWD_PATHS: usize = 8;

    #[derive(Clone, PartialEq, Eq)]
    struct ShellState {
        cwd: PathBuf,
        physical: Option<bool>,
        snapshot_unchanged: bool,
    }
    #[derive(Clone)]
    enum Paths {
        Known(Vec<ShellState>),
        // The folder is unknown, but its default cd mode may still be known.
        Unknown(Option<bool>),
    }
    impl Paths {
        fn empty() -> Self {
            Self::Known(Vec::new())
        }
        fn one(cwd: PathBuf, physical: bool) -> Self {
            Self::Known(vec![ShellState {
                cwd,
                physical: Some(physical),
                snapshot_unchanged: true,
            }])
        }
        fn is_empty(&self) -> bool {
            matches!(self, Self::Known(paths) if paths.is_empty())
        }
        fn mode(&self) -> Option<bool> {
            match self {
                Self::Unknown(mode) => *mode,
                Self::Known(states) => {
                    let first = states.first()?.physical;
                    if states.iter().all(|state| state.physical == first) {
                        first
                    } else {
                        None
                    }
                }
            }
        }
        fn insert(&mut self, state: ShellState) {
            match self {
                Self::Unknown(mode) => {
                    if *mode != state.physical {
                        *mode = None;
                    }
                }
                Self::Known(paths) if !paths.contains(&state) => {
                    // Too many possible shell states make the set unknown; never discard
                    // just the extra folder and keep pretending the set is complete.
                    if paths.len() >= MAX_CWD_PATHS {
                        let mode = paths
                            .iter()
                            .all(|path| path.physical == state.physical)
                            .then_some(state.physical)
                            .flatten();
                        *self = Self::Unknown(mode);
                    } else {
                        paths.push(state);
                    }
                }
                _ => {}
            }
        }
        fn union(mut self, other: Self) -> Self {
            if self.is_empty() {
                return other;
            }
            match other {
                Self::Unknown(mode) => Self::Unknown(if self.mode() == mode { mode } else { None }),
                Self::Known(paths) => {
                    for path in paths {
                        self.insert(path);
                    }
                    self
                }
            }
        }
        fn with_physical(self, physical: Option<bool>) -> Self {
            match self {
                Self::Unknown(_) => Self::Unknown(physical),
                Self::Known(states) => {
                    let mut result = Self::empty();
                    for mut state in states {
                        state.physical = physical;
                        result.insert(state);
                    }
                    result
                }
            }
        }
        fn snapshot_unchanged(&self) -> bool {
            match self {
                Self::Known(states) => states.iter().all(|state| state.snapshot_unchanged),
                Self::Unknown(_) => false,
            }
        }
        fn invalidate_snapshot(mut self) -> Self {
            if let Self::Known(states) = &mut self {
                for state in states {
                    state.snapshot_unchanged = false;
                }
            }
            self
        }
    }
    struct Flow {
        success: Paths,
        failure: Paths,
    }
    impl Flow {
        fn success(paths: Paths) -> Self {
            Self {
                success: paths,
                failure: Paths::empty(),
            }
        }
        fn failure(paths: Paths) -> Self {
            Self {
                success: Paths::empty(),
                failure: paths,
            }
        }
        fn either(paths: Paths) -> Self {
            Self {
                success: paths.clone(),
                failure: paths,
            }
        }
        fn all(self) -> Paths {
            self.success.union(self.failure)
        }
    }
    enum Operator {
        Sequence,
        And,
        Or,
    }
    enum SetModeChange {
        Unchanged,
        Set(bool),
        Unknown,
    }

    fn literal_word(raw: &str) -> Option<String> {
        let mut parsed = crate::bash_rewrite::parser::parse(raw)?;
        (parsed.args.len() == 1 && parsed.appends_to.is_none() && parsed.heredoc.is_none())
            .then(|| parsed.args.remove(0))
    }
    fn followable_pushd(parts: &[Part]) -> bool {
        let mut args = parts.iter().skip(1);
        let Some(first) = args.next() else {
            return false;
        };
        let options_ended = literal_word(&first.text).as_deref() == Some("--");
        let operand = if options_ended {
            args.next()
        } else {
            Some(first)
        };
        let Some(operand) = operand.filter(|_| args.next().is_none()) else {
            return false;
        };
        let raw = &operand.text;
        let word = if raw == "~" || raw.starts_with("~/") {
            literal_word(&format!("\\{raw}"))
        } else {
            literal_word(raw)
        };
        let Some(word) = word else {
            return false;
        };
        let negative_index = word.strip_prefix('-').is_some_and(|index| {
            !index.is_empty() && index.chars().all(|character| character.is_ascii_digit())
        });
        word != "-"
            && !word.starts_with('+')
            && !negative_index
            && (options_ended || !word.starts_with('-'))
    }
    /// Non-literal arguments while parsing `set` options may change physical mode.
    fn set_mode_change(parts: &[Part]) -> SetModeChange {
        let mut change = SetModeChange::Unchanged;
        let mut args = parts.iter().skip(1);
        while let Some(part) = args.next() {
            let Some(word) = literal_word(&part.text) else {
                return SetModeChange::Unknown;
            };
            if matches!(word.as_str(), "--" | "-" | "+") {
                break;
            }
            if !word.starts_with(['-', '+']) {
                break;
            }
            let enabled = word.starts_with('-');
            let mut flags = word[1..].chars().peekable();
            while let Some(flag) = flags.next() {
                match flag {
                    'P' => change = SetModeChange::Set(enabled),
                    'o' => {
                        if flags.peek().is_some() {
                            return SetModeChange::Unknown;
                        }
                        let Some(option) = args.next() else {
                            return change;
                        };
                        let Some(option) = literal_word(&option.text) else {
                            return SetModeChange::Unknown;
                        };
                        if option == "physical" {
                            change = SetModeChange::Set(enabled);
                        } else if !matches!(
                            option.as_str(),
                            "allexport"
                                | "braceexpand"
                                | "emacs"
                                | "errexit"
                                | "errtrace"
                                | "functrace"
                                | "hashall"
                                | "histexpand"
                                | "history"
                                | "ignoreeof"
                                | "keyword"
                                | "monitor"
                                | "noclobber"
                                | "noexec"
                                | "noglob"
                                | "nolog"
                                | "notify"
                                | "nounset"
                                | "onecmd"
                                | "pipefail"
                                | "posix"
                                | "privileged"
                                | "verbose"
                                | "vi"
                                | "xtrace"
                        ) {
                            return SetModeChange::Unknown;
                        }
                    }
                    flag if "abefhkmnptuvxBCEHT".contains(flag) => {}
                    _ => return SetModeChange::Unknown,
                }
            }
        }
        change
    }
    fn resolve_target(
        head: &str,
        parts: &[Part],
        cwd: Option<&Path>,
        default_physical: bool,
    ) -> Option<(DirectoryTarget, PathBuf)> {
        let mut physical = default_physical;
        let mut options = true;
        let mut operand = None;
        for part in parts.iter().skip(1) {
            let word = literal_word(&part.text);
            if options {
                if word.as_deref() == Some("--") {
                    options = false;
                    continue;
                }
                if let Some(flags) = word
                    .as_deref()
                    .filter(|word| word.starts_with('-') && word.len() > 1)
                {
                    if head == "cd" {
                        // Bash processes option letters in order; the last -L/-P wins.
                        for flag in flags.chars().skip(1) {
                            match flag {
                                'L' => physical = false,
                                'P' => physical = true,
                                'e' | '@' => {}
                                _ => return None,
                            }
                        }
                    }
                    continue;
                }
            }
            if operand.replace(part.text.as_str()).is_some() {
                return None;
            }
            options = false;
        }
        let raw = operand?;
        let expand_home = raw == "~" || raw.starts_with("~/");
        // Decode the supported home prefix without allowing other expansions;
        // quoted or escaped tildes remain ordinary path text.
        let word = if expand_home {
            format!("\\{raw}")
        } else {
            raw.to_owned()
        };
        let text = literal_word(&word)
            .filter(|text| text != "-" && !(head == "pushd" && text.starts_with('+')))?;
        let literal_absolute = Path::new(&text).is_absolute();
        let joined = if expand_home {
            PathBuf::from(std::env::var_os("HOME")?).join(text.strip_prefix("~/").unwrap_or(""))
        } else if literal_absolute {
            PathBuf::from(text)
        } else {
            cwd?.join(text)
        };
        let (path, next_cwd) = if physical {
            let resolved = super::resolve_with_existing_ancestors(&joined);
            (resolved.clone(), resolved)
        } else {
            // Default cd follows logical PWD: fold .. before resolving symlinks
            // for the containment check, and retain that logical spelling to walk.
            let logical = super::normalize_path(&joined);
            (super::resolve_with_existing_ancestors(&logical), logical)
        };
        Some((
            DirectoryTarget {
                path,
                alternate_path: None,
                literal_absolute,
            },
            next_cwd,
        ))
    }
    fn directory_change(
        head: &str,
        parts: &[Part],
        incoming: Paths,
        targets: &mut Vec<DirectoryTarget>,
        known_directory: &dyn Fn(&Path) -> bool,
    ) -> Flow {
        if head == "pushd" && !followable_pushd(parts) {
            // Stack rotation or an unmodeled stack edit cannot establish a
            // successful cwd. Failure still leaves the incoming folder intact.
            return Flow {
                success: Paths::Unknown(incoming.mode()),
                failure: incoming,
            };
        }
        let mut success = Paths::empty();
        let mut failure = Paths::empty();
        let mut emitted: Vec<DirectoryTarget> = Vec::new();
        let mut resolve = |cwd, mode, snapshot_unchanged| {
            let resolved = match mode {
                Some(physical) => resolve_target(head, parts, cwd, physical)
                    .map(|(target, next_cwd)| (target, Some(next_cwd))),
                None => resolve_target(head, parts, cwd, false)
                    .zip(resolve_target(head, parts, cwd, true))
                    .map(|((mut logical, logical_cwd), (physical, physical_cwd))| {
                        if logical.path != physical.path {
                            logical.alternate_path = Some(physical.path);
                        }
                        // A divergent logical/physical walk cannot establish the
                        // next folder, even if one candidate is allowed remotely.
                        let next_cwd = (logical_cwd == physical_cwd).then_some(logical_cwd);
                        (logical, next_cwd)
                    }),
            };
            if let Some((target, next_cwd)) = resolved {
                let known_success = snapshot_unchanged
                    && known_directory(&target.path)
                    && target
                        .alternate_path
                        .as_ref()
                        .is_none_or(|path| known_directory(path));
                if let Some(next_cwd) = next_cwd {
                    success.insert(ShellState {
                        cwd: next_cwd,
                        physical: mode,
                        snapshot_unchanged,
                    });
                } else {
                    success = Paths::Unknown(None);
                }
                if !emitted.contains(&target) {
                    emitted.push(target);
                }
                known_success
            } else {
                success = success.clone().union(Paths::Unknown(mode));
                false
            }
        };
        match &incoming {
            Paths::Known(paths) => {
                for state in paths {
                    if !resolve(
                        Some(state.cwd.as_path()),
                        state.physical,
                        state.snapshot_unchanged,
                    ) {
                        failure.insert(state.clone());
                    }
                }
            }
            Paths::Unknown(mode) => {
                if !resolve(None, *mode, false) {
                    failure = incoming.clone();
                }
            }
        }
        targets.extend(emitted);
        // Missing or unresolved destinations may still fail and leave the cwd unchanged.
        Flow { success, failure }
    }
    fn inspect_independent_children(
        node: Node<'_>,
        source: &str,
        targets: &mut Vec<DirectoryTarget>,
        known_directory: &dyn Fn(&Path) -> bool,
    ) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            let _ = evaluate(
                child,
                source,
                Paths::Unknown(None),
                targets,
                known_directory,
            );
        }
    }
    fn has_substitution(node: Node<'_>) -> bool {
        if matches!(node.kind(), "command_substitution" | "process_substitution") {
            return true;
        }
        let mut cursor = node.walk();
        let found = node.named_children(&mut cursor).any(has_substitution);
        found
    }
    fn evaluate(
        node: Node<'_>,
        source: &str,
        incoming: Paths,
        targets: &mut Vec<DirectoryTarget>,
        known_directory: &dyn Fn(&Path) -> bool,
    ) -> Flow {
        if incoming.is_empty() {
            return Flow::success(incoming);
        }
        match node.kind() {
            "subshell" => return Flow::either(Paths::Unknown(incoming.mode())),
            "command_substitution" | "process_substitution" => {
                return Flow::either(incoming.invalidate_snapshot());
            }
            "function_definition" => return Flow::success(incoming.invalidate_snapshot()),
            "if_statement"
            | "for_statement"
            | "c_style_for_statement"
            | "while_statement"
            | "case_statement"
            | "pipeline" => {
                // Unsupported control flow loses cwd precision; independently
                // resolvable targets remain inspectable without choosing a branch.
                inspect_independent_children(node, source, targets, known_directory);
                return Flow::either(Paths::Unknown(None));
            }
            "negated_command" => {
                if let Some(body) = node.named_child(0) {
                    let inner = evaluate(body, source, incoming, targets, known_directory);
                    return Flow {
                        success: inner.failure,
                        failure: inner.success,
                    };
                }
                return Flow::either(incoming);
            }
            "redirected_statement" => {
                let incoming = incoming.invalidate_snapshot();
                let mut cursor = node.walk();
                let body = node.child_by_field_name("body").or_else(|| {
                    node.named_children(&mut cursor)
                        .find(|child| !child.kind().contains("redirect"))
                });
                let inner = body
                    .map(|body| evaluate(body, source, incoming.clone(), targets, known_directory))
                    .unwrap_or_else(|| Flow::either(incoming.clone()));
                // Redirection can fail before the body changes directory.
                return Flow {
                    success: inner.success,
                    failure: inner.failure.union(incoming),
                };
            }
            "command" => {
                let parts = command_parts(source, node);
                let head = parts.first().and_then(|part| literal_word(&part.text));
                let incoming = if head.as_deref() == Some("cd") && !has_substitution(node) {
                    incoming
                } else {
                    incoming.invalidate_snapshot()
                };
                return match head.as_deref() {
                    Some(head @ ("cd" | "pushd")) => {
                        directory_change(head, &parts, incoming, targets, known_directory)
                    }
                    Some("popd") => Flow {
                        success: Paths::Unknown(incoming.mode()),
                        failure: incoming,
                    },
                    Some("source" | "." | "eval") | None => Flow::either(Paths::Unknown(None)),
                    Some("set") => {
                        let flow = match set_mode_change(&parts) {
                            SetModeChange::Unchanged => Flow::success(incoming.clone()),
                            SetModeChange::Set(physical) => {
                                Flow::success(incoming.clone().with_physical(Some(physical)))
                            }
                            SetModeChange::Unknown => {
                                Flow::either(incoming.clone().with_physical(None))
                            }
                        };
                        let mut cursor = node.walk();
                        if node
                            .named_children(&mut cursor)
                            .any(|child| child.kind().contains("redirect"))
                        {
                            Flow {
                                success: flow.success,
                                failure: flow.failure.union(incoming),
                            }
                        } else {
                            flow
                        }
                    }
                    Some("false") => Flow::failure(incoming),
                    Some("true" | ":") => {
                        let mut cursor = node.walk();
                        if node
                            .named_children(&mut cursor)
                            .any(|child| child.kind().contains("redirect"))
                        {
                            Flow::either(incoming)
                        } else {
                            Flow::success(incoming)
                        }
                    }
                    _ => Flow::either(incoming),
                };
            }
            _ => {}
        }
        let mut flow: Option<Flow> = None;
        let mut operator = Operator::Sequence;
        let mut asynchronous_entry = incoming.clone();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "&&" => {
                    operator = Operator::And;
                    continue;
                }
                "||" => {
                    operator = Operator::Or;
                    continue;
                }
                "&" => {
                    // The asynchronous list ran in a child shell. Its final cwd
                    // and options must not replace the foreground entry state.
                    flow = flow.map(|left| {
                        // Background jobs share files with the foreground shell,
                        // even though their directory changes do not carry over.
                        let entry = if left.success.snapshot_unchanged()
                            && left.failure.snapshot_unchanged()
                        {
                            asynchronous_entry.clone()
                        } else {
                            asynchronous_entry.clone().invalidate_snapshot()
                        };
                        Flow::success(entry)
                    });
                    operator = Operator::Sequence;
                    continue;
                }
                "comment" => continue,
                _ if !child.is_named() => continue,
                _ => {}
            }
            flow = Some(match flow.take() {
                None => evaluate(child, source, incoming.clone(), targets, known_directory),
                Some(left) => match operator {
                    Operator::Sequence => {
                        asynchronous_entry = left.all();
                        evaluate(
                            child,
                            source,
                            asynchronous_entry.clone(),
                            targets,
                            known_directory,
                        )
                    }
                    Operator::And => {
                        let right = evaluate(child, source, left.success, targets, known_directory);
                        Flow {
                            success: right.success,
                            failure: left.failure.union(right.failure),
                        }
                    }
                    Operator::Or => {
                        let right = evaluate(child, source, left.failure, targets, known_directory);
                        Flow {
                            success: left.success.union(right.success),
                            failure: right.failure,
                        }
                    }
                },
            });
            operator = Operator::Sequence;
        }
        flow.unwrap_or_else(|| Flow::either(incoming))
    }

    let mut targets = Vec::new();
    if let Some(tree) = parse_tree(command) {
        let _ = evaluate(
            tree.root_node(),
            command,
            Paths::one(cwd.to_path_buf(), false),
            &mut targets,
            known_directory,
        );
    }
    targets
}

fn command_parts(source: &str, node: Node<'_>) -> Vec<Part> {
    let mut out = Vec::new();

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "command_elements" {
            let mut element_cursor = child.walk();
            for item in child.children(&mut element_cursor) {
                if item.kind() == "command_argument_sep" || item.kind() == "redirection" {
                    continue;
                }
                out.push(Part {
                    text: node_text(source, item).to_string(),
                });
            }
            continue;
        }

        if matches!(
            child.kind(),
            "command_name"
                | "command_name_expr"
                | "word"
                | "string"
                | "raw_string"
                | "concatenation"
                | "number"
        ) {
            out.push(Part {
                text: node_text(source, child).to_string(),
            });
        }
    }

    out
}

fn source(command: &str, node: Node<'_>) -> String {
    let node = node
        .parent()
        .filter(|parent| parent.kind() == "redirected_statement")
        .unwrap_or(node);
    node_text(command, node).trim().to_string()
}

fn node_text<'source>(source: &'source str, node: Node<'_>) -> &'source str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

enum RedirectTarget {
    /// A resolved redirect target. `input_only` is true only for a plain `<`
    /// redirect, which reads the file and never writes it.
    Path {
        path: PathBuf,
        input_only: bool,
    },
    RelativeToUnknown,
    Dynamic,
}

enum PathArgTarget {
    Path(PathBuf),
    RelativeToUnknown,
    Dynamic,
    None,
}

fn collect_redirection_targets(
    source: &str,
    command: Node<'_>,
    cwd: Option<&Path>,
    mut on_target: impl FnMut(RedirectTarget),
) {
    // In tree-sitter-bash, redirects on `echo hi > /tmp/out` are siblings of
    // the `command` node, wrapped inside `redirected_statement`. Walking
    // `command.children` would miss them. Lists and pipelines can also carry
    // this wrapper: only their last command owns the trailing redirect, in
    // the directory in effect for that command.
    let mut body = command;
    while let Some(parent) = body.parent() {
        match parent.kind() {
            "redirected_statement" => {
                let mut cursor = parent.walk();
                for child in parent.children(&mut cursor) {
                    if child.id() != body.id() {
                        collect_redirection_targets_from_node(source, child, cwd, &mut on_target);
                    }
                }
                return;
            }
            "list" | "pipeline" if is_last_named_child(parent, body) => body = parent,
            _ => break,
        }
    }
    let mut cursor = command.walk();
    for child in command.children(&mut cursor) {
        collect_redirection_targets_from_node(source, child, cwd, &mut on_target);
    }
}

fn is_last_named_child(parent: Node<'_>, child: Node<'_>) -> bool {
    parent
        .named_child_count()
        .checked_sub(1)
        .and_then(|last| parent.named_child(last as u32))
        .is_some_and(|last| last.id() == child.id())
}

fn collect_redirection_targets_from_node(
    source: &str,
    node: Node<'_>,
    cwd: Option<&Path>,
    on_target: &mut impl FnMut(RedirectTarget),
) {
    // tree-sitter-bash 0.25.x emits `file_redirect` and `heredoc_redirect`
    // (and `redirection` in some shapes). All three carry a redirect target
    // as a child `word`/`file`/`raw_string`/`string`/`concatenation`/
    // `simple_expansion`/`expansion`/`command_substitution`.
    if matches!(
        node.kind(),
        "file_redirect" | "heredoc_redirect" | "herestring_redirect" | "redirection"
    ) {
        let input_only = node.kind() == "file_redirect" && {
            let mut cursor = node.walk();
            let is_plain_input = node
                .children(&mut cursor)
                .any(|child| !child.is_named() && child.kind() == "<");
            is_plain_input
        };
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if is_dynamic_node(child) {
                on_target(RedirectTarget::Dynamic);
                continue;
            }
            if matches!(
                child.kind(),
                "word" | "file" | "raw_string" | "string" | "concatenation"
            ) {
                let text = node_text(source, child);
                match path_arg_target(text, cwd) {
                    PathArgTarget::Path(path) => {
                        on_target(RedirectTarget::Path { path, input_only })
                    }
                    PathArgTarget::RelativeToUnknown => {
                        on_target(RedirectTarget::RelativeToUnknown);
                    }
                    PathArgTarget::Dynamic => on_target(RedirectTarget::Dynamic),
                    PathArgTarget::None => {}
                }
            }
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_redirection_targets_from_node(source, child, cwd, on_target);
    }
}

fn is_dynamic_node(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "expansion" | "command_substitution" | "simple_expansion"
    )
}

/// Follow directory changes, marking an unknowable destination as unknown
/// until an absolute literal change (or bare `cd` to HOME) restores the base.
fn track_cwd_change(
    head: &str,
    parts: &[Part],
    home: Option<&Path>,
    scan_cwd: &mut Option<PathBuf>,
    directory_stack: &mut Vec<Option<PathBuf>>,
) {
    let args = parts.iter().skip(1).map(|part| part.text.as_str());
    let options_only = args.clone().all(|arg| arg.starts_with('-') && arg != "-");
    let target = |cwd: &Option<PathBuf>| {
        path_args(parts)
            .next()
            .and_then(|arg| arg_path(arg, cwd.as_deref()))
    };
    match head {
        "cd" => {
            *scan_cwd = if options_only {
                // A bare `cd` goes to the home directory.
                home.map(resolve_existing)
            } else if args.clone().any(|arg| arg == "-") {
                None
            } else {
                target(scan_cwd)
            };
        }
        "pushd" => {
            let rotates = options_only
                || args.clone().any(|arg| unquote(arg).starts_with(['+', '-']))
                || path_args(parts).count() != 1;
            let next = if rotates {
                None
            } else {
                target(scan_cwd).filter(|path| path.is_dir())
            };
            if next.is_none() {
                // Rotation reorders the stack rather than pushing the old cwd.
                // Unmodeled forms cannot leave saved entries that a later popd
                // might mistake for a known directory.
                directory_stack.clear();
                *scan_cwd = None;
            } else {
                directory_stack.push(std::mem::replace(scan_cwd, next));
            }
        }
        "popd" => {
            *scan_cwd = if parts.len() == 1 {
                directory_stack.pop().flatten()
            } else {
                directory_stack.clear();
                None
            };
        }
        _ => {}
    }
}

fn path_args(parts: &[Part]) -> impl Iterator<Item = &str> {
    let head = parts.first().map(|part| part.text.as_str()).unwrap_or("");
    parts.iter().skip(1).filter_map(move |part| {
        if part.text.starts_with('-') || (head == "chmod" && part.text.starts_with('+')) {
            None
        } else {
            Some(part.text.as_str())
        }
    })
}

fn arg_path(arg: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    match path_arg_target(arg, cwd) {
        PathArgTarget::Path(path) => Some(path),
        PathArgTarget::RelativeToUnknown | PathArgTarget::Dynamic | PathArgTarget::None => None,
    }
}

fn path_arg_target(arg: &str, cwd: Option<&Path>) -> PathArgTarget {
    let text = home(&unquote(arg), cwd);
    if dynamic(&text) {
        return PathArgTarget::Dynamic;
    }

    if cwd.is_none()
        && !text.is_empty()
        && !Path::new(&text).is_absolute()
        && !is_device_file(&text)
    {
        return PathArgTarget::RelativeToUnknown;
    }

    let Some(text) = glob_prefix(&text) else {
        return PathArgTarget::None;
    };

    // Check the raw string BEFORE PathBuf construction/canonicalization:
    // - on Linux /dev/stderr etc. are symlinks (/proc/self/fd/2 -> /dev/pts/0)
    //   that no longer match the allowlist once resolved;
    // - on Windows, `cwd.join("/dev/null")` mangles the path to `C:\dev\null`,
    //   so a PathBuf-based check never matches (the PR #107 tests failed on
    //   Windows CI for exactly this reason). The agent typed a POSIX device
    //   path; match it as the string it is.
    if is_device_file(text) {
        return PathArgTarget::None;
    }

    let path = PathBuf::from(text);
    let resolved = if path.is_absolute() {
        path
    } else {
        match cwd {
            Some(cwd) => cwd.join(path),
            None => return PathArgTarget::None,
        }
    };
    PathArgTarget::Path(resolve_existing(&resolved))
}

/// Remove shell quotes from one word, including words built from several
/// quoted and unquoted segments such as `"/etc"/passwd`. Backslashes are kept
/// as typed: the same scanner sees Windows paths, where they separate
/// components rather than escape the next character.
fn unquote(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut quote = None;
    for ch in text.chars() {
        match quote {
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            Some(open) if ch == open => quote = None,
            _ => out.push(ch),
        }
    }
    out
}

fn home(text: &str, cwd: Option<&Path>) -> String {
    if text == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| text.to_string());
    }
    if let Some(rest) = text.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return Path::new(&home).join(rest).to_string_lossy().into_owned();
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let text = text.replace("$HOME", &home).replace("${HOME}", &home);
    match cwd {
        Some(cwd) => {
            let pwd = cwd.to_string_lossy();
            text.replace("$PWD", pwd.as_ref())
                .replace("${PWD}", pwd.as_ref())
        }
        None => text,
    }
}

fn glob_prefix(text: &str) -> Option<&str> {
    match text.find(['?', '*', '[']) {
        Some(0) => None,
        Some(index) => Some(&text[..index]),
        None => Some(text),
    }
}

fn dynamic(text: &str) -> bool {
    text.starts_with('(')
        || text.contains("$(")
        || text.contains("${")
        || text.contains('`')
        || text.contains('$')
}

fn resolve_existing(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| super::normalize_path(path))
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn push_bash_ask(
    asks: &mut Vec<PermissionAsk>,
    seen: &mut HashSet<String>,
    pattern: String,
    tokens: &[String],
) {
    let stable = arity::prefix(tokens).join(" ");
    if stable.is_empty() {
        return;
    }
    push_ask(
        asks,
        seen,
        PermissionAsk {
            kind: PermissionKind::Bash,
            patterns: vec![pattern],
            always: vec![format!("{stable} *")],
            reason: None,
            targets: Vec::new(),
        },
    );
}

fn push_xargs_ask(asks: &mut Vec<PermissionAsk>, seen: &mut HashSet<String>, tokens: &[String]) {
    let mut index = 1;
    while index < tokens.len() && tokens[index].starts_with('-') {
        index += 1;
    }
    if index >= tokens.len() {
        return;
    }
    push_bash_ask(asks, seen, tokens[index..].join(" "), &tokens[index..]);
}

/// Match POSIX device-file paths on the RAW string the agent typed (not a
/// canonicalized PathBuf — see the call site for why). `/dev/fd/` accepts
/// only bare numeric descriptors: `/dev/fd/../sda` resolves to a real block
/// device, and the scanner must fail closed on anything path-shaped.
fn is_device_file(s: &str) -> bool {
    matches!(
        s,
        "/dev/null"
            | "/dev/zero"
            | "/dev/random"
            | "/dev/urandom"
            | "/dev/stdin"
            | "/dev/stdout"
            | "/dev/stderr"
            | "/dev/tty"
    ) || s
        .strip_prefix("/dev/fd/")
        .is_some_and(|fd| !fd.is_empty() && fd.bytes().all(|b| b.is_ascii_digit()))
}

fn push_external_path(
    asks: &mut Vec<PermissionAsk>,
    seen: &mut HashSet<String>,
    project_root: &Path,
    path: &Path,
    read_exemption: Option<ReadExemption<'_>>,
) {
    if path.starts_with(project_root) || super::is_system_temp_path(path) {
        return;
    }
    if read_exemption.is_some_and(|is_exempt| is_exempt(path)) {
        return;
    }
    use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
    let grant = AccessGrant {
        path: path.to_path_buf(),
        kind: if path.is_dir() {
            GrantKind::Subtree
        } else {
            GrantKind::File
        },
        // Folder eligibility is identical for read and write approvals.
        operation: AccessOperation::Read,
    };
    let scope = crate::subc::directory_scope(&grant);
    let pattern = match scope.kind {
        GrantKind::File => display_path(&scope.path),
        GrantKind::Subtree => format!("{}/*", display_path(&scope.path)),
    };
    push_ask(
        asks,
        seen,
        PermissionAsk {
            kind: PermissionKind::ExternalDirectory,
            patterns: vec![pattern.clone()],
            always: vec![pattern],
            reason: None,
            targets: vec![grant],
        },
    );
}

fn push_ask(asks: &mut Vec<PermissionAsk>, seen: &mut HashSet<String>, ask: PermissionAsk) {
    let key = format!("{:?}:{:?}:{:?}", ask.kind, ask.patterns, ask.always);
    if seen.insert(key) {
        asks.push(ask);
    } else if let Some(existing) = asks.iter_mut().find(|existing| {
        existing.kind == ask.kind
            && existing.patterns == ask.patterns
            && existing.always == ask.always
    }) {
        // A single displayed folder ask may represent several files. Keep all
        // of them so approving one file cannot authorize its unapproved sibling.
        for target in ask.targets {
            if !existing
                .targets
                .iter()
                .any(|prior| same_external_target(prior, &target))
            {
                existing.targets.push(target);
            }
        }
    }
}

fn same_external_target(
    left: &crate::access_policy::AccessGrant,
    right: &crate::access_policy::AccessGrant,
) -> bool {
    #[cfg(windows)]
    let same_path = crate::windows_path::normalize_windows_path(&left.path)
        == crate::windows_path::normalize_windows_path(&right.path);
    #[cfg(not(windows))]
    let same_path = left.path == right.path;
    same_path && left.kind == right.kind && left.operation == right.operation
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory_paths(command: &str, cwd: &Path) -> Vec<PathBuf> {
        top_level_directory_targets(command, cwd, &|_| false)
            .into_iter()
            .map(|target| target.path)
            .collect()
    }

    fn external_ask_shapes(command: &str, project: &Path) -> Vec<(Vec<String>, Vec<String>)> {
        scan_with_project_root(command, project, project)
            .into_iter()
            .filter(|ask| matches!(ask.kind, PermissionKind::ExternalDirectory))
            .map(|ask| (ask.patterns, ask.always))
            .collect()
    }

    #[test]
    fn directory_targets_preserve_literal_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        let absolute = dir.path().canonicalize().unwrap();
        let command = format!("cd '{}' && cd nested && cd '~/literal'", absolute.display());
        let targets = top_level_directory_targets(&command, &absolute, &|_| false);
        assert_eq!(
            targets
                .iter()
                .map(|target| target.literal_absolute)
                .collect::<Vec<_>>(),
            [true, false, false]
        );
    }

    #[test]
    fn directory_targets_background_lists_preserve_foreground() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let cases = [
            (
                "cd nested & cd child & cd ..",
                vec![
                    cwd.join("nested"),
                    cwd.join("child"),
                    cwd.parent().unwrap().to_path_buf(),
                ],
            ),
            (
                "false && cd nested & cd ..",
                vec![cwd.parent().unwrap().to_path_buf()],
            ),
            ("sleep 1 & false && cd ..", vec![]),
            (
                "cd nested && { sleep 1 & cd ..; }",
                vec![cwd.join("nested"), cwd.clone()],
            ),
            ("sleep 1 & cd ..", vec![cwd.parent().unwrap().to_path_buf()]),
            (
                "cd nested & cd ..",
                vec![cwd.join("nested"), cwd.parent().unwrap().to_path_buf()],
            ),
            (
                "cd nested && sleep 1 & cd ..",
                vec![cwd.join("nested"), cwd.parent().unwrap().to_path_buf()],
            ),
            (
                "cd nested; sleep 1 & cd ..",
                vec![
                    cwd.join("nested"),
                    cwd.clone(),
                    cwd.parent().unwrap().to_path_buf(),
                ],
            ),
        ];
        for (command, expected) in cases {
            assert_eq!(directory_paths(command, &cwd), expected, "{command}");
        }
    }

    #[test]
    fn directory_targets_preserve_success_and_failure_reachability() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let parent = cwd.parent().unwrap().to_path_buf();
        assert_eq!(
            directory_paths("false && cd nested; cd ..", &cwd),
            vec![parent.clone()]
        );
        assert_eq!(
            directory_paths("true || cd nested; cd ..", &cwd),
            vec![parent.clone()]
        );
        assert_eq!(
            directory_paths("maybe && cd nested; cd ..", &cwd),
            vec![cwd.join("nested"), cwd.clone(), parent.clone()]
        );
        assert_eq!(
            directory_paths("cd nested || cd fallback", &cwd),
            vec![cwd.join("nested"), cwd.join("fallback")]
        );
        assert_eq!(
            directory_paths("! cd nested && cd ..", &cwd),
            vec![cwd.join("nested"), parent.clone()]
        );
        assert!(directory_paths("false && cd ../not-executed", &cwd).is_empty());
        assert!(directory_paths("true || cd ../not-executed", &cwd).is_empty());
        assert_eq!(
            directory_paths("true >/not-writable && cd nested; cd ..", &cwd),
            vec![cwd.join("nested"), cwd.clone(), parent]
        );
    }

    #[test]
    fn directory_targets_only_drop_failures_for_known_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        std::fs::create_dir(cwd.join("a")).unwrap();
        std::fs::create_dir(cwd.join("b")).unwrap();
        let known_directory = |path: &Path| path.starts_with(&cwd) && path.is_dir();
        for (command, expected) in [
            ("cd a; cd ../b", vec![cwd.join("a"), cwd.join("b")]),
            (
                "maybe && cd a; cd ../b",
                vec![
                    cwd.join("a"),
                    cwd.join("b"),
                    cwd.parent().unwrap().join("b"),
                ],
            ),
            (
                "cd missing; cd ../b",
                vec![
                    cwd.join("missing"),
                    cwd.join("b"),
                    cwd.parent().unwrap().join("b"),
                ],
            ),
            ("cd a || cd ..", vec![cwd.join("a")]),
            (
                "cd a >/not-writable || cd ..",
                vec![cwd.join("a"), cwd.parent().unwrap().to_path_buf()],
            ),
        ] {
            let paths: Vec<_> = top_level_directory_targets(command, &cwd, &known_directory)
                .into_iter()
                .map(|target| target.path)
                .collect();
            assert_eq!(paths, expected, "{command}");
        }
    }

    #[test]
    fn directory_targets_only_prove_supported_cd_invocations() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        for name in ["a", "-Z"] {
            std::fs::create_dir(cwd.join(name)).unwrap();
        }
        for (command, destination) in [
            ("cd a || cd ..", "a"),
            ("cd -e a || cd ..", "a"),
            ("cd -@ a || cd ..", "a"),
            ("cd '-LP' -e@ -- a || cd ..", "a"),
            ("cd -- -Z || cd ..", "-Z"),
        ] {
            let targets = top_level_directory_targets(command, &cwd, &|path| path.is_dir());
            assert_eq!(targets.len(), 1, "{command}");
            assert_eq!(targets[0].path, cwd.join(destination), "{command}");
        }
        for command in [
            "cd a extra || cd ..",
            "cd -Z a || cd ..",
            "cd -PZ a || cd ..",
            "cd -- a extra || cd ..",
        ] {
            let targets = top_level_directory_targets(command, &cwd, &|path| path.is_dir());
            assert_eq!(targets.len(), 1, "{command}");
            assert_eq!(targets[0].path, cwd.parent().unwrap(), "{command}");
        }
    }

    #[test]
    fn directory_targets_bound_reachable_cwd_alternatives() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let command = format!(
            "cd a; cd b; cd c; cd d; cd ..; cd '{}' && cd child",
            cwd.display()
        );
        let targets = directory_paths(&command, &cwd);
        assert_eq!(targets.len(), 17);
        assert_eq!(&targets[15..], &[cwd.clone(), cwd.join("child")]);
    }

    #[test]
    fn directory_targets_follow_successive_literal_changes() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        assert_eq!(
            directory_paths("cd nested && cd ..", &cwd),
            vec![cwd.join("nested"), cwd.clone()]
        );
        assert_eq!(
            directory_paths("pushd nested && cd ../..", &cwd),
            vec![cwd.join("nested"), cwd.parent().unwrap().to_path_buf()]
        );
        assert_eq!(
            directory_paths("pushd -- nested && cd ../..", &cwd),
            vec![cwd.join("nested"), cwd.parent().unwrap().to_path_buf()]
        );
    }

    #[test]
    fn directory_targets_stop_resolving_relative_paths_after_unknown_changes() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        for prefix in [
            "cd \"$X\"",
            "pushd \"$X\"",
            "cd -",
            "pushd +1",
            "popd",
            "(cd nested)",
        ] {
            let command = format!("{prefix} && cd ..; cd '{}' && cd nested", cwd.display());
            assert_eq!(
                directory_paths(&command, &cwd),
                vec![cwd.clone(), cwd.join("nested")],
                "{prefix}"
            );
        }
    }

    #[test]
    fn directory_targets_unfollowable_pushd_forgets_successful_folder() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        for change in [
            "pushd +1",
            "pushd -1",
            "pushd -- -1",
            "pushd",
            "pushd -n /usr",
            "pushd -1 ignored",
            "pushd /usr extra",
            "pushd \"$UNKNOWN\"",
        ] {
            for separator in [";", "&&"] {
                let command = format!("{change} {separator} cd child");
                assert!(directory_paths(&command, &cwd).is_empty(), "{command}");
            }
            // A failed pushd leaves the cwd unchanged; a background pushd must
            // not change the foreground shell's cwd.
            for command in [
                format!("{change} || cd child"),
                format!("{change} & cd child"),
            ] {
                assert_eq!(
                    directory_paths(&command, &cwd),
                    vec![cwd.join("child")],
                    "{command}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn directory_targets_stack_unknown_preserves_physical_mode_and_absolute_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let deep = root.join("deep");
        std::fs::create_dir(&deep).unwrap();
        std::os::unix::fs::symlink("..", deep.join("link")).unwrap();
        for (option, last) in [
            ("-P", root.parent().unwrap().to_path_buf()),
            ("-o physical", root.parent().unwrap().to_path_buf()),
            ("+P", deep.clone()),
            ("+o physical", deep.clone()),
        ] {
            let command = format!(
                "set {option}; pushd -1 ignored; cd '{}' && cd link && cd ..",
                deep.display()
            );
            assert_eq!(
                directory_paths(&command, &deep),
                vec![deep.clone(), root.clone(), last],
                "{command}"
            );
        }
    }

    #[test]
    fn quoted_heredoc_body_is_not_scanned_as_commands_or_paths() {
        let scratch = std::env::current_dir().unwrap().join("target/bash-heredoc");
        std::fs::create_dir_all(&scratch).unwrap();
        let fixture = tempfile::tempdir_in(scratch).unwrap();
        let project = fixture.path().join("project");
        let outside = fixture.path().join("outside");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(project.join("gate.sh"), "").unwrap();
        let command = format!(
            "cat > '{}/gate.sh' <<'EOF'\ncp '{}/body-only.txt' /tmp/copy\nsed -n '1p' '{}/body-only.txt'\npython3 - <<'PY'\nprint('$(body_only_command /tmp/data)')\nPY\necho \"$HOME $(body_only_command '{}/body-only.txt')\"\nEOF\n",
            project.display(), outside.display(), outside.display(), outside.display(),
        );
        let asks = scan_with_project_root(&command, &project, &outside);
        assert_eq!(
            asks.len(),
            1,
            "only the outer cat command may ask: {asks:?}"
        );
        assert_eq!(asks[0].kind, PermissionKind::Bash);
        assert_eq!(asks[0].always, ["cat *"]);
        assert!(asks[0].patterns[0].starts_with("cat > "));
    }

    #[test]
    fn unknown_cwd_preserves_individual_command_permissions() {
        let project = tempfile::tempdir().unwrap();
        let command = "cd \"$DEST\" && rm -rf build";
        let asks = scan_with_project_root(command, project.path(), project.path());
        let rm = asks
            .iter()
            .find(|ask| ask.patterns == ["rm -rf build"])
            .expect("the host must still evaluate rm command rules");
        assert_eq!(rm.kind, PermissionKind::Bash);
        assert_eq!(rm.always, ["rm *"]);
        assert!(rm.reason.is_none());
        let exact = asks
            .iter()
            .find(|ask| ask.reason.as_deref() == Some(UNKNOWN_CWD_PERMISSION_REASON))
            .expect("the unknown directory still needs exact-command approval");
        assert_eq!(exact.kind, PermissionKind::Bash);
        assert_eq!(exact.patterns, [command]);
        assert!(exact.always.is_empty());
        assert!(asks
            .iter()
            .all(|ask| ask.kind != PermissionKind::ExternalDirectory));
    }

    #[test]
    fn relative_paths_after_an_unknown_directory_change_ask() {
        let project = tempfile::tempdir().unwrap();
        for command in [
            "cd \"$TARGET\" && rm -rf build",
            "cd - && rm notes.txt",
            "popd && rm notes.txt",
            "pushd +1 && rm notes.txt",
            "pushd && rm notes.txt",
            "cd \"$TARGET\" && cd sub && rm notes.txt",
            "cd \"$TARGET\" && echo hi > out.txt",
            "cd \"$TARGET\"; cat 'quoted name'/file",
        ] {
            assert_unknown_cwd_ask(command, project.path());
        }
    }

    fn assert_unknown_cwd_ask(command: &str, project: &Path) {
        let asks = scan_with_project_root(command, project, project);
        let exact = asks
            .iter()
            .find(|ask| ask.reason.as_deref() == Some(UNKNOWN_CWD_PERMISSION_REASON))
            .unwrap();
        assert_eq!(exact.patterns, [command], "{command}");
        assert!(exact.always.is_empty(), "{command}");
        assert_eq!(exact.reason.as_deref(), Some(UNKNOWN_CWD_PERMISSION_REASON));
        assert!(asks.iter().all(|ask| ask.kind != PermissionKind::ExternalDirectory || !ask.targets.is_empty()), "{asks:?}");
    }

    #[cfg(unix)]
    #[test]
    fn rotated_directory_stack_cannot_restore_a_stale_project_directory() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join("+1")).unwrap();
        std::fs::create_dir(project.path().join("-1")).unwrap();
        let project_path = project.path().display();
        for change in [
            "pushd +1",
            "pushd -1",
            "pushd '+1'",
            "pushd '-1'",
            "pushd",
            "pushd /usr extra",
            "pushd missing-directory",
            "pushd \"$UNKNOWN\"",
            "popd +1",
            "popd -1",
            "popd -n",
        ] {
            let command = format!(
                "pushd /usr; pushd /etc; pushd '{project_path}'; {change}; popd; rm notes.txt"
            );
            assert_unknown_cwd_ask(&command, project.path());
        }
    }

    #[test]
    fn numeric_directory_stack_options_are_preserved() {
        let command = "popd -1";
        let tree = parse_tree(command).unwrap();
        let mut commands = Vec::new();
        collect_commands(tree.root_node(), &mut commands);
        assert_eq!(commands.len(), 1);
        let parts = command_parts(command, commands[0]);
        assert_eq!(
            parts
                .iter()
                .map(|part| part.text.as_str())
                .collect::<Vec<_>>(),
            ["popd", "-1"]
        );
    }

    #[test]
    fn directory_targets_preserve_conditional_cwd_alternatives() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        assert_eq!(
            directory_paths("cd nested || printf fallback; cd ..", &cwd),
            vec![
                cwd.join("nested"),
                cwd.clone(),
                cwd.parent().unwrap().to_path_buf()
            ]
        );
        for command in [
            "if true; then cd nested; fi; cd ..",
            "while false; do cd nested; done; cd ..",
            "cd nested | cat; cd ..",
        ] {
            assert!(directory_paths(command, &cwd).is_empty(), "{command}");
        }
        let command = format!("if false; then cd '{}'; fi; cd ..", cwd.display());
        assert_eq!(directory_paths(&command, &cwd), vec![cwd]);
    }

    #[test]
    fn top_level_directory_targets_resolve_lists_and_options_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let targets = directory_paths(
            "cd -- nested && printf ok; pushd '../sibling repo'; cd ./other/..",
            &cwd,
        );
        assert_eq!(
            targets,
            vec![
                cwd.join("nested"),
                cwd.join("sibling repo"),
                cwd.parent().unwrap().join("sibling repo"),
                cwd.join("sibling repo"),
                cwd.parent().unwrap().join("sibling repo"),
                cwd.join("nested"),
                cwd
            ]
        );
    }

    #[test]
    fn top_level_directory_targets_ignore_dynamic_and_nested_commands() {
        let dir = tempfile::tempdir().unwrap();
        for command in [
            "cd $OTHER_CHECKOUT",
            "cd \"${OTHER_CHECKOUT}\"",
            "cd $(printf /outside)",
            "cd -",
            "pushd +1",
            "cd ../*",
            "printf '%s' \"$(cd /outside; pwd)\"",
            "(cd /outside; printf nested)",
            "f() { cd /outside; }; printf defined",
        ] {
            assert!(directory_paths(command, dir.path()).is_empty(), "{command}");
        }
    }

    #[test]
    fn top_level_directory_targets_keep_quoted_tilde_literal() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        assert_eq!(
            directory_paths("cd '~/nested'", &cwd),
            vec![cwd.join("~/nested")]
        );
        assert_eq!(
            directory_paths(
                "cd ../sibling\\ repo; cd '../literal*'; cd '../$literal'",
                &cwd
            ),
            vec![
                cwd.parent().unwrap().join("sibling repo"),
                cwd.parent().unwrap().join("literal*"),
                cwd.parent().unwrap().join("$literal")
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_targets_track_shell_physical_mode_and_async_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let deep = root.join("deep");
        std::fs::create_dir(&deep).unwrap();
        std::os::unix::fs::symlink("..", deep.join("link")).unwrap();
        let parent = root.parent().unwrap().to_path_buf();
        for (command, expected) in [
            (
                "set -P; cd link && cd ..",
                vec![root.clone(), parent.clone()],
            ),
            (
                "set -o physical; cd link && cd ..",
                vec![root.clone(), parent.clone()],
            ),
            (
                "set -P; set +P; cd link && cd ..",
                vec![root.clone(), deep.clone()],
            ),
            (
                "set -o physical; set +o physical; cd link && cd ..",
                vec![root.clone(), deep.clone()],
            ),
            (
                "set -P; cd -L link && cd ..",
                vec![root.clone(), parent.clone()],
            ),
            (
                "set -P; cd -L link && cd -L ..",
                vec![root.clone(), deep.clone()],
            ),
            (
                "set -- -P; cd link && cd ..",
                vec![root.clone(), deep.clone()],
            ),
            (
                "set value -P; cd link && cd ..",
                vec![root.clone(), deep.clone()],
            ),
            (
                "set -eP; cd link && cd ..",
                vec![root.clone(), parent.clone()],
            ),
            (
                "set -P +P; cd link && cd ..",
                vec![root.clone(), deep.clone()],
            ),
            (
                "false && set -P; cd link && cd ..",
                vec![root.clone(), deep.clone()],
            ),
            (
                "true || set -P; cd link && cd ..",
                vec![root.clone(), deep.clone()],
            ),
            (
                "set -P; false || cd link && cd ..",
                vec![root.clone(), parent.clone()],
            ),
            (
                "set -P & cd link && cd ..",
                vec![root.clone(), deep.clone()],
            ),
            (
                "set -P; set +P & cd link && cd ..",
                vec![root.clone(), parent.clone()],
            ),
            (
                "set -P && : & cd link && cd ..",
                vec![root.clone(), deep.clone()],
            ),
            ("cd link && : & cd ..", vec![root.clone(), root.clone()]),
            ("set -o; cd link && cd ..", vec![root.clone(), deep.clone()]),
        ] {
            let output = std::process::Command::new("/bin/bash")
                .args([
                    "--noprofile",
                    "--norc",
                    "-c",
                    &format!("{command} && pwd -P; code=$?; wait; exit \"$code\""),
                ])
                .current_dir(&deep)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .output()
                .unwrap();
            assert!(output.status.success(), "{command}: {output:?}");
            assert_eq!(
                String::from_utf8(output.stdout)
                    .unwrap()
                    .lines()
                    .last()
                    .unwrap()
                    .trim(),
                expected.last().unwrap().to_str().unwrap(),
                "{command}"
            );
            assert_eq!(directory_paths(command, &deep), expected, "{command}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn directory_targets_unknown_set_preserves_folder_but_not_mode() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let deep = root.join("deep");
        std::fs::create_dir(&deep).unwrap();
        std::os::unix::fs::symlink("..", deep.join("link")).unwrap();
        for prefix in [
            "source ./script",
            "eval \"$X\"",
            "set \"$OPTS\"",
            "set -o \"$OPT\"",
            "set -o unsupported",
            "set -P \"$OPTS\"",
        ] {
            let expected = if prefix.starts_with("set ") {
                vec![root.clone()]
            } else {
                vec![]
            };
            assert_eq!(
                directory_paths(&format!("{prefix}; cd link && cd .."), &deep),
                expected,
                "{prefix}"
            );
            let logical = format!(
                "{prefix}; set +P; cd '{}' && cd link && cd ..",
                deep.display()
            );
            assert_eq!(
                directory_paths(&logical, &deep),
                vec![deep.clone(), root.clone(), deep.clone()],
                "{prefix}"
            );
            let physical = format!(
                "{prefix}; set -P; cd '{}' && cd link && cd ..",
                deep.display()
            );
            assert_eq!(
                directory_paths(&physical, &deep),
                vec![
                    deep.clone(),
                    root.clone(),
                    root.parent().unwrap().to_path_buf()
                ],
                "{prefix}"
            );
        }
        assert_eq!(
            directory_paths("set \"$OPTS\"; cd .. && cd ..", &deep),
            vec![root.clone(), root.parent().unwrap().to_path_buf()]
        );
        let paired =
            top_level_directory_targets("set \"$OPTS\"; cd link/.. && cd ../..", &deep, &|_| false);
        assert_eq!(paired.len(), 1);
        assert_eq!(paired[0].path, deep);
        assert_eq!(
            paired[0].alternate_path,
            Some(root.parent().unwrap().to_path_buf())
        );
        let explicit =
            top_level_directory_targets("set \"$OPTS\"; cd -P link/..", &deep, &|_| false);
        assert_eq!(explicit.len(), 1);
        assert_eq!(explicit[0].path, root.parent().unwrap());
        assert_eq!(explicit[0].alternate_path, None);
        for both_known in [false, true] {
            let targets = top_level_directory_targets(
                "set \"$OPTS\"; cd link/.. || cd fallback",
                &deep,
                &|path| both_known || path.starts_with(&root),
            );
            assert_eq!(targets.len(), 2);
            assert_eq!(targets[1].path, deep.join("fallback"));
        }
        for command in ["set -- \"$X\"; cd ..", "set value \"$X\"; cd .."] {
            assert_eq!(
                directory_paths(command, &deep),
                vec![root.clone()],
                "{command}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn directory_targets_preserve_logical_paths_and_physical_cd_flags() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let deep = root.join("deep");
        std::fs::create_dir(&deep).unwrap();
        std::os::unix::fs::symlink("..", deep.join("link")).unwrap();
        let parent = root.parent().unwrap().to_path_buf();
        for (command, expected) in [
            ("cd link && cd ..", vec![root.clone(), deep.clone()]),
            (
                "pushd link >/dev/null && cd ..",
                vec![root.clone(), deep.clone()],
            ),
            ("cd -- link && cd ..", vec![root.clone(), deep.clone()]),
            ("cd link/..", vec![deep.clone()]),
            ("cd -P link && cd ..", vec![root.clone(), parent.clone()]),
            ("cd -P link/..", vec![parent.clone()]),
            ("cd -P -L link && cd ..", vec![root.clone(), deep.clone()]),
            ("cd -L -P link && cd ..", vec![root.clone(), parent.clone()]),
            ("cd -PL link && cd ..", vec![root.clone(), deep.clone()]),
            ("cd -LP link && cd ..", vec![root.clone(), parent.clone()]),
            ("cd '-P' link && cd ..", vec![root.clone(), parent.clone()]),
        ] {
            let output = std::process::Command::new("/bin/bash")
                .args([
                    "--noprofile",
                    "--norc",
                    "-c",
                    &format!("{command} && pwd -P"),
                ])
                .current_dir(&deep)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .output()
                .unwrap();
            assert!(output.status.success(), "{command}: {output:?}");
            assert_eq!(
                String::from_utf8(output.stdout).unwrap().trim(),
                expected.last().unwrap().to_str().unwrap(),
                "{command}"
            );
            assert_eq!(directory_paths(command, &deep), expected, "{command}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn top_level_directory_targets_resolve_symlinks_even_with_missing_children() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("checkout");
        let sibling = dir.path().join("sibling");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::os::unix::fs::symlink(&sibling, checkout.join("link")).unwrap();
        assert_eq!(
            directory_paths("cd link/missing", &checkout),
            vec![sibling.canonicalize().unwrap().join("missing")]
        );
    }

    #[test]
    fn unknown_directory_never_produces_a_blanket_ask() {
        let project = tempfile::tempdir().unwrap();
        assert!(external_ask_shapes("cd \"$TARGET\" && rm \"$NAME\"", project.path()).is_empty());
        assert!(
            external_ask_shapes("cd \"$TARGET\" && make 2>&1 > /dev/null", project.path())
                .is_empty()
        );
        assert_unknown_cwd_ask("cd \"$TARGET\" && rm -rf build '*/*.txt'", project.path());
    }

    #[cfg(unix)]
    #[test]
    fn literal_absolute_directory_change_ends_the_unknown_state() {
        let project = tempfile::tempdir().unwrap();
        let usr = vec![(
            vec!["/usr/aft-scan-probe".to_string()],
            vec!["/usr/aft-scan-probe".to_string()],
        )];
        assert_eq!(
            external_ask_shapes(
                "cd \"$TARGET\" && cd /usr && rm aft-scan-probe",
                project.path()
            ),
            usr
        );
        assert_eq!(
            external_ask_shapes("cd \"$TARGET\" && rm /usr/aft-scan-probe", project.path()),
            usr
        );
        let inside = project.path().display().to_string();
        assert!(external_ask_shapes(
            &format!("cd \"$TARGET\" && cd '{inside}' && rm notes.txt"),
            project.path()
        )
        .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn trailing_redirect_after_a_list_belongs_to_the_last_command() {
        let project = tempfile::tempdir().unwrap();
        for (command, target) in [
            ("cd /usr && echo hi > out.txt", "/usr/out.txt"),
            (
                "true && echo hi > /usr/aft-scan-probe",
                "/usr/aft-scan-probe",
            ),
            ("echo hi | cat > /usr/aft-scan-probe", "/usr/aft-scan-probe"),
        ] {
            assert_eq!(
                external_ask_shapes(command, project.path()),
                vec![(vec![target.to_string()], vec![target.to_string()])],
                "{command}"
            );
        }
        assert!(external_ask_shapes("echo hi > out.txt && cd /usr", project.path()).is_empty());
    }
    use crate::bash_permissions::normalize_path;

    #[cfg(windows)]
    #[test]
    fn external_target_dedup_normalizes_windows_path_spellings() {
        use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
        for (plain, verbatim) in [
            (r"D:/a/aft/file.txt", r"\\?\d:\a\aft\file.txt"),
            (
                r"\\server\share\folder\file.txt",
                r"\\?\UNC\server\share\folder\file.txt",
            ),
        ] {
            let original = AccessGrant {
                path: PathBuf::from(verbatim),
                kind: GrantKind::File,
                operation: AccessOperation::Read,
            };
            let pattern = "unchanged-host-pattern/*";
            let ask = |target| PermissionAsk {
                kind: PermissionKind::ExternalDirectory,
                patterns: vec![pattern.into()],
                always: vec![pattern.into()],
                reason: None,
                targets: vec![target],
            };
            let mut asks = Vec::new();
            let mut seen = HashSet::new();
            push_ask(&mut asks, &mut seen, ask(original.clone()));
            push_ask(
                &mut asks,
                &mut seen,
                ask(AccessGrant {
                    path: PathBuf::from(plain),
                    ..original.clone()
                }),
            );
            assert_eq!(asks.len(), 1);
            assert_eq!(
                asks[0].targets,
                [original.clone()],
                "equivalent targets should merge without rewriting the first spelling"
            );
            push_ask(
                &mut asks,
                &mut seen,
                ask(AccessGrant {
                    kind: GrantKind::Subtree,
                    ..original.clone()
                }),
            );
            push_ask(
                &mut asks,
                &mut seen,
                ask(AccessGrant {
                    operation: AccessOperation::Write,
                    ..original.clone()
                }),
            );
            push_ask(
                &mut asks,
                &mut seen,
                ask(AccessGrant {
                    path: original.path.with_file_name("other.txt"),
                    ..original
                }),
            );
            assert_eq!(
                asks[0].targets.len(),
                4,
                "different kind, mode or path must remain distinct"
            );
            assert_eq!(asks[0].patterns, [pattern]);
            assert_eq!(asks[0].always, [pattern]);
        }
    }

    #[test]
    fn pwd_expands_against_scan_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let scan_cwd = temp.path().join("project").join("subdir");
        std::fs::create_dir_all(&scan_cwd).unwrap();

        let path = arg_path("$PWD/output.log", Some(&scan_cwd)).unwrap();

        assert_eq!(path, normalize_path(&scan_cwd.join("output.log")));
    }

    #[test]
    fn dynamic_file_arg_is_reported_as_dynamic_target() {
        let target = path_arg_target(r#""$DEST/file""#, Some(Path::new("/tmp")));

        assert!(matches!(target, PathArgTarget::Dynamic));
    }

    #[test]
    fn relative_redirect_target_resolves_against_scan_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let scan_cwd = temp.path().join("outside");
        std::fs::create_dir_all(&scan_cwd).unwrap();

        let path = arg_path("./file.log", Some(&scan_cwd)).unwrap();

        assert_eq!(path, normalize_path(&scan_cwd.join("file.log")));
    }

    #[test]
    fn dev_fd_accepts_only_numeric_descriptors() {
        // `/dev/fd/2` is a process file descriptor; `/dev/fd/../sda` resolves
        // to a real block device. The scanner fails closed on the latter.
        for arg in ["/dev/fd/0", "/dev/fd/2", "/dev/fd/255"] {
            assert!(
                matches!(
                    path_arg_target(arg, Some(Path::new("/tmp"))),
                    PathArgTarget::None
                ),
                "expected {arg} to be skipped as a device path"
            );
        }
        for arg in ["/dev/fd/../sda", "/dev/fd/", "/dev/fd/2x", "/dev/fd/2/x"] {
            assert!(
                matches!(
                    path_arg_target(arg, Some(Path::new("/tmp"))),
                    PathArgTarget::Path(_)
                ),
                "expected {arg} to be treated as a regular path (fail closed)"
            );
        }
    }

    #[test]
    fn device_paths_are_skipped_before_canonicalization() {
        // On Linux /dev/stderr is a symlink chain ending at the TTY (or a
        // pipe inode under CI), so the device check must run on the raw
        // path — after canonicalize it no longer matches the allowlist.
        for arg in ["/dev/stderr", "/dev/stdin", "/dev/stdout", "/dev/fd/2"] {
            let target = path_arg_target(arg, Some(Path::new("/tmp")));
            assert!(
                matches!(target, PathArgTarget::None),
                "expected {arg} to be skipped as a device path"
            );
        }
    }

    #[test]
    fn dev_null_redirect_does_not_trigger_permission_ask() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(project_root.clone()),
                bash_permissions: true,
                ..crate::config::Config::default()
            },
        );

        let asks = scan_with_cwd("echo test 2>/dev/null", &ctx, &project_root);

        let external_asks: Vec<_> = asks
            .iter()
            .filter(|ask| matches!(ask.kind, PermissionKind::ExternalDirectory))
            .collect();
        assert!(
            external_asks.is_empty(),
            "Expected no external directory asks for /dev/null redirect, got: {:?}",
            external_asks
        );
        // The absence above is only meaningful if the scanner is discriminating
        // rather than silent: a scanner stubbed to return nothing would satisfy
        // it. Pin the positive half — the command itself stays gated.
        assert!(
            asks.iter()
                .any(|ask| matches!(ask.kind, PermissionKind::Bash)
                    && ask.patterns.iter().any(|pattern| pattern.contains("echo"))),
            "the echo command must still produce a command-specific bash ask: {asks:?}"
        );
    }

    #[test]
    fn dev_stderr_redirect_does_not_trigger_permission_ask() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(project_root.clone()),
                bash_permissions: true,
                ..crate::config::Config::default()
            },
        );

        let asks = scan_with_cwd("some-command 2>/dev/stderr", &ctx, &project_root);

        let external_asks: Vec<_> = asks
            .iter()
            .filter(|ask| matches!(ask.kind, PermissionKind::ExternalDirectory))
            .collect();
        assert!(
            external_asks.is_empty(),
            "Expected no external directory asks for /dev/stderr redirect, got: {:?}",
            external_asks
        );
        // See dev_null_redirect_does_not_trigger_permission_ask: pin the
        // positive half so a silent scanner cannot satisfy the absence.
        assert!(
            asks.iter()
                .any(|ask| matches!(ask.kind, PermissionKind::Bash)
                    && ask
                        .patterns
                        .iter()
                        .any(|pattern| pattern.contains("some-command"))),
            "the command must still produce a command-specific bash ask: {asks:?}"
        );
    }

    /// An external path outside the project must produce an external_directory
    /// ask. The sibling tests all assert that specific paths do NOT produce one
    /// (device files, /dev/null, temp dirs), so deleting the ask entirely — as
    /// opposed to inverting the guard that suppresses it — satisfies every one
    /// of them. Guards have two failure directions and only the suppression
    /// direction was covered here.
    // Unix-only: the target must be outside BOTH the project and the system
    // temp tree (tempfile roots live under the temp tree, which is exempt by
    // design), and /etc is the portable Unix path that satisfies both.
    #[cfg(unix)]
    fn external_asks(command: &str, project: &Path) -> Vec<String> {
        scan_with_project_root(command, project, project)
            .into_iter()
            .filter(|ask| matches!(ask.kind, PermissionKind::ExternalDirectory))
            .flat_map(|ask| ask.patterns)
            .collect()
    }

    // Temporary directories never need an external-directory ask, so these
    // tests point at `/usr`, which exists outside every test project.
    #[cfg(unix)]
    #[test]
    fn partly_quoted_absolute_path_is_not_treated_as_relative() {
        let project = tempfile::tempdir().unwrap();
        let asks = external_asks(r#"rm "/usr"/aft-scan-probe"#, project.path());
        assert_eq!(asks, vec!["/usr/aft-scan-probe".to_string()]);
        assert_eq!(unquote(r#""/etc"/'pass'wd"#), "/etc/passwd");
        assert_eq!(unquote(r#"'a"b'"c'd""#), "a\"bc'd");
    }

    #[cfg(unix)]
    #[test]
    fn pushd_and_popd_move_the_scan_directory() {
        let project = tempfile::tempdir().unwrap();
        let asks = external_asks("pushd /usr && rm aft-scan-probe", project.path());
        assert_eq!(asks, vec!["/usr/aft-scan-probe".to_string()]);
        let asks = external_asks("pushd /usr && popd && rm aft-scan-probe", project.path());
        assert!(asks.is_empty(), "{asks:?}");
    }

    #[test]
    fn bare_cd_moves_the_scan_directory_home() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut scan_cwd = Some(temp.path().to_path_buf());
        let mut stack = Vec::new();
        let parts = |words: &[&str]| {
            words
                .iter()
                .map(|word| Part {
                    text: word.to_string(),
                })
                .collect::<Vec<_>>()
        };
        track_cwd_change(
            "cd",
            &parts(&["cd", "-P"]),
            Some(&home),
            &mut scan_cwd,
            &mut stack,
        );
        assert_eq!(scan_cwd, Some(resolve_existing(&home)));
        track_cwd_change(
            "cd",
            &parts(&["cd", "-"]),
            Some(&home),
            &mut scan_cwd,
            &mut stack,
        );
        assert_eq!(scan_cwd, None, "cd - goes to an unknown directory");
    }

    #[test]
    fn external_path_outside_the_project_still_asks() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(project_root.clone()),
                bash_permissions: true,
                ..crate::config::Config::default()
            },
        );

        let asks = scan_with_cwd("cat /etc/hosts", &ctx, &project_root);

        assert!(
            asks.iter()
                .any(|ask| matches!(ask.kind, PermissionKind::ExternalDirectory)),
            "reading outside the project must require an external_directory ask: {asks:?}"
        );
    }

    /// A scanner stubbed to one blanket ask must fail this, which is what the
    /// sibling absence assertions cannot detect alone: every vector in this
    /// module expects an ask, so a constant-ask implementation satisfies all of
    /// them. Discriminating behavior is the property under test — different
    /// commands, different asks.
    #[test]
    fn scanner_discriminates_between_commands_rather_than_asking_blanket() {
        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(project_root.clone()),
                bash_permissions: true,
                ..crate::config::Config::default()
            },
        );

        let git = scan_with_cwd("git status", &ctx, &project_root);
        let echo = scan_with_cwd("echo hello", &ctx, &project_root);

        let patterns = |asks: &[PermissionAsk]| {
            asks.iter()
                .flat_map(|ask| ask.patterns.clone())
                .collect::<Vec<_>>()
        };
        let git_patterns = patterns(&git);
        let echo_patterns = patterns(&echo);

        assert_ne!(
            git_patterns, echo_patterns,
            "different commands must yield different asks; identical patterns mean the \
             scanner is not reading the command: git={git_patterns:?} echo={echo_patterns:?}"
        );
        assert!(
            git_patterns.iter().any(|pattern| pattern.contains("git")),
            "the git ask must name git: {git_patterns:?}"
        );
        assert!(
            !git_patterns.iter().any(|pattern| pattern == "*"),
            "a well-formed command must not fall back to the wildcard ask: {git_patterns:?}"
        );
    }
}
