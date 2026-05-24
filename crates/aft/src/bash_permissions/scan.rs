use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use tree_sitter::{Node, Parser};

use crate::context::AppContext;

use super::{arity, PermissionAsk, PermissionKind};

const FILE_COMMANDS: &[&str] = &[
    "rm", "cp", "mv", "mkdir", "touch", "chmod", "chown", "cat", "cd", "source", ".",
];
const CWD_COMMANDS: &[&str] = &["cd", "pushd", "popd"];

#[derive(Debug, Clone)]
struct Part {
    text: String,
}

pub fn scan(command: &str, ctx: &AppContext) -> Vec<PermissionAsk> {
    #[cfg(windows)]
    {
        let _ = (command, ctx);
        return Vec::new();
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
            if let Some(arg) = path_args(&parts).next() {
                if let Some(path) = arg_path(arg, &scan_cwd) {
                    scan_cwd = path;
                }
            }
            continue;
        }

        if FILE_COMMANDS.contains(&head) {
            for arg in path_args(&parts) {
                let Some(path) = arg_path(arg, &scan_cwd) else {
                    continue;
                };
                push_external_path(&mut asks, &mut seen, &project_root, &path);
            }
        }

        collect_redirection_targets(command, node, &scan_cwd, |target| match target {
            RedirectTarget::Path(path) => {
                push_external_path(&mut asks, &mut seen, &project_root, &path);
            }
            RedirectTarget::Dynamic => push_external_wildcard(&mut asks, &mut seen),
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
    Path(PathBuf),
    Dynamic,
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
                if dynamic(text) {
                    on_target(RedirectTarget::Dynamic);
                } else if let Some(path) = arg_path(text, cwd) {
                    on_target(RedirectTarget::Path(path));
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
    let text = home(&unquote(arg));
    let text = glob_prefix(&text)?;
    if dynamic(text) {
        return None;
    }

    let path = PathBuf::from(text);
    let resolved = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    Some(resolve_existing(&resolved))
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

fn home(text: &str) -> String {
    if text == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| text.to_string());
    }
    if let Some(rest) = text.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return Path::new(&home).join(rest).to_string_lossy().into_owned();
        }
    }
    text.replace("$HOME", &std::env::var("HOME").unwrap_or_default())
        .replace("${HOME}", &std::env::var("HOME").unwrap_or_default())
        .replace(
            "$PWD",
            &std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy(),
        )
        .replace(
            "${PWD}",
            &std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy(),
        )
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
    std::fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    result.push(component.as_os_str());
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
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

fn push_external_path(
    asks: &mut Vec<PermissionAsk>,
    seen: &mut HashSet<String>,
    project_root: &Path,
    path: &Path,
) {
    if path.starts_with(project_root) {
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

fn push_external_wildcard(asks: &mut Vec<PermissionAsk>, seen: &mut HashSet<String>) {
    push_ask(
        asks,
        seen,
        PermissionAsk {
            kind: PermissionKind::ExternalDirectory,
            patterns: vec!["*".to_string()],
            always: vec!["*".to_string()],
        },
    );
}

fn push_ask(asks: &mut Vec<PermissionAsk>, seen: &mut HashSet<String>, ask: PermissionAsk) {
    let key = format!("{:?}:{:?}:{:?}", ask.kind, ask.patterns, ask.always);
    if seen.insert(key) {
        asks.push(ask);
    }
}
