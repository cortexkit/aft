use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tree_sitter::{Node, Parser};

use crate::context::AppContext;

use super::{arity, PermissionAsk, PermissionKind};

const FILE_COMMANDS: &[&str] = &[
    "rm", "cp", "mv", "mkdir", "touch", "chmod", "chown", "cat", "cd", "source", ".",
];
const CWD_COMMANDS: &[&str] = &["cd", "pushd", "popd"];
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

    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        // Fail closed: if we can't even load the bash grammar we cannot
        // verify the command is safe, so require explicit permission via
        // a wildcard ask rather than silently letting the command run.
        // This was previously `return Vec::new()` which created a hard
        // bypass of the user's bash permission rules whenever grammar
        // loading failed.
        return vec![parse_failed_ask()];
    }

    let Some(tree) = parser.parse(command, None) else {
        return vec![parse_failed_ask()];
    };

    let root = tree.root_node();
    if root.has_error() {
        return vec![parse_failed_ask()];
    }
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
    let mut scan_cwd = cwd.clone();
    for node in command_nodes {
        let parts = command_parts(command, node);
        if parts.is_empty() {
            continue;
        }

        let tokens: Vec<String> = parts.iter().map(|part| part.text.clone()).collect();
        let head = tokens[0].as_str();

        if head == "cd" {
            collect_redirection_targets(command, node, &scan_cwd, |target| match target {
                RedirectTarget::Path { path, input_only } => {
                    let exemption = if input_only { read_exemption } else { None };
                    push_external_path(&mut asks, &mut seen, &project_root, &path, exemption);
                }
                RedirectTarget::Dynamic => {}
            });
            if let Some(arg) = path_args(&parts).next() {
                if let Some(path) = arg_path(arg, &scan_cwd) {
                    scan_cwd = path;
                }
            }
            continue;
        }

        if FILE_COMMANDS.contains(&head) {
            let exemption = if READ_ONLY_FILE_COMMANDS.contains(&head) {
                read_exemption
            } else {
                None
            };
            for arg in path_args(&parts) {
                match path_arg_target(arg, &scan_cwd) {
                    PathArgTarget::Path(path) => {
                        push_external_path(&mut asks, &mut seen, &project_root, &path, exemption);
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

        collect_redirection_targets(command, node, &scan_cwd, |target| match target {
            RedirectTarget::Path { path, input_only } => {
                // Only a plain `<` input redirect reads its target; `>`, `>>`,
                // `&>`, `<>` and friends can create or truncate it.
                let exemption = if input_only { read_exemption } else { None };
                push_external_path(&mut asks, &mut seen, &project_root, &path, exemption);
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

    asks
}

fn parse_failed_ask() -> PermissionAsk {
    PermissionAsk {
        kind: PermissionKind::Bash,
        patterns: vec!["*".to_string()],
        always: Vec::new(),
    }
}

fn collect_commands<'tree>(node: Node<'tree>, out: &mut Vec<Node<'tree>>) {
    if node.kind() == "command" {
        out.push(node);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_commands(child, out);
    }
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
    Dynamic,
}

enum PathArgTarget {
    Path(PathBuf),
    Dynamic,
    None,
}

fn collect_redirection_targets(
    source: &str,
    command: Node<'_>,
    cwd: &Path,
    mut on_target: impl FnMut(RedirectTarget),
) {
    // In tree-sitter-bash, redirects on `echo hi > /tmp/out` are siblings of
    // the `command` node, wrapped inside `redirected_statement`. Walking
    // `command.children` would miss them. Walk from the parent
    // (`redirected_statement`) when present, otherwise fall back to walking
    // `command` itself in case a future grammar version inlines redirections
    // under `command`.
    let walk_root = match command.parent() {
        Some(parent) if parent.kind() == "redirected_statement" => parent,
        _ => command,
    };
    let mut cursor = walk_root.walk();
    for child in walk_root.children(&mut cursor) {
        // Skip the inner `command` node itself — its children are arguments,
        // not redirections, and re-walking them is just wasted work.
        if child.id() == command.id() {
            continue;
        }
        collect_redirection_targets_from_node(source, child, cwd, &mut on_target);
    }
}

fn collect_redirection_targets_from_node(
    source: &str,
    node: Node<'_>,
    cwd: &Path,
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

fn arg_path(arg: &str, cwd: &Path) -> Option<PathBuf> {
    match path_arg_target(arg, cwd) {
        PathArgTarget::Path(path) => Some(path),
        PathArgTarget::Dynamic | PathArgTarget::None => None,
    }
}

fn path_arg_target(arg: &str, cwd: &Path) -> PathArgTarget {
    let text = home(&unquote(arg), cwd);
    if dynamic(&text) {
        return PathArgTarget::Dynamic;
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
        cwd.join(path)
    };
    PathArgTarget::Path(resolve_existing(&resolved))
}

fn unquote(text: &str) -> String {
    if text.len() < 2 {
        return text.to_string();
    }
    let bytes = text.as_bytes();
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if (first == b'\'' || first == b'"') && first == last {
        text[1..text.len() - 1].to_string()
    } else {
        text.to_string()
    }
}

fn home(text: &str, cwd: &Path) -> String {
    if text == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| text.to_string());
    }
    if let Some(rest) = text.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return Path::new(&home).join(rest).to_string_lossy().into_owned();
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let pwd = cwd.to_string_lossy();
    text.replace("$HOME", &home)
        .replace("${HOME}", &home)
        .replace("$PWD", pwd.as_ref())
        .replace("${PWD}", pwd.as_ref())
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

fn permission_dir(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().unwrap_or(path).to_path_buf()
    }
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
    let dir = permission_dir(path);
    let pattern = format!("{}/*", display_path(&dir));
    push_ask(
        asks,
        seen,
        PermissionAsk {
            kind: PermissionKind::ExternalDirectory,
            patterns: vec![pattern.clone()],
            always: vec![pattern],
        },
    );
}

fn push_ask(asks: &mut Vec<PermissionAsk>, seen: &mut HashSet<String>, ask: PermissionAsk) {
    let key = format!("{:?}:{:?}:{:?}", ask.kind, ask.patterns, ask.always);
    if seen.insert(key) {
        asks.push(ask);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bash_permissions::normalize_path;

    #[test]
    fn pwd_expands_against_scan_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let scan_cwd = temp.path().join("project").join("subdir");
        std::fs::create_dir_all(&scan_cwd).unwrap();

        let path = arg_path("$PWD/output.log", &scan_cwd).unwrap();

        assert_eq!(path, normalize_path(&scan_cwd.join("output.log")));
    }

    #[test]
    fn dynamic_file_arg_is_reported_as_dynamic_target() {
        let target = path_arg_target(r#""$DEST/file""#, Path::new("/tmp"));

        assert!(matches!(target, PathArgTarget::Dynamic));
    }

    #[test]
    fn relative_redirect_target_resolves_against_scan_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let scan_cwd = temp.path().join("outside");
        std::fs::create_dir_all(&scan_cwd).unwrap();

        let path = arg_path("./file.log", &scan_cwd).unwrap();

        assert_eq!(path, normalize_path(&scan_cwd.join("file.log")));
    }

    #[test]
    fn dev_fd_accepts_only_numeric_descriptors() {
        // `/dev/fd/2` is a process file descriptor; `/dev/fd/../sda` resolves
        // to a real block device. The scanner fails closed on the latter.
        for arg in ["/dev/fd/0", "/dev/fd/2", "/dev/fd/255"] {
            assert!(
                matches!(path_arg_target(arg, Path::new("/tmp")), PathArgTarget::None),
                "expected {arg} to be skipped as a device path"
            );
        }
        for arg in ["/dev/fd/../sda", "/dev/fd/", "/dev/fd/2x", "/dev/fd/2/x"] {
            assert!(
                matches!(
                    path_arg_target(arg, Path::new("/tmp")),
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
            let target = path_arg_target(arg, Path::new("/tmp"));
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
