//! Best-effort literal write targets for the runon preflight, not a shell sandbox.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser};

use super::scan::{command_parts, walk_shell_commands};

#[derive(Clone, Copy)]
enum Shape {
    Tee,
    InPlace,
    Destination,
    Remove,
    Touch,
    Patch,
    Git,
    Formatter,
    Interpreter,
}

// All command-shape classification lives here. Unknown commands and dynamic
// operands are left to the build server's filesystem protection.
const SHAPES: &[(&str, Shape)] = &[
    ("tee", Shape::Tee),
    ("sed", Shape::InPlace),
    ("perl", Shape::InPlace),
    ("ruby", Shape::InPlace),
    ("cp", Shape::Destination),
    ("mv", Shape::Destination),
    ("install", Shape::Destination),
    ("ln", Shape::Destination),
    ("rm", Shape::Remove),
    ("unlink", Shape::Remove),
    ("truncate", Shape::Remove),
    ("touch", Shape::Touch),
    ("patch", Shape::Patch),
    ("git", Shape::Git),
    ("cargo", Shape::Formatter),
    ("rustfmt", Shape::Formatter),
    ("prettier", Shape::Formatter),
    ("biome", Shape::Formatter),
    ("gofmt", Shape::Formatter),
    ("black", Shape::Formatter),
    ("ruff", Shape::Formatter),
    ("python", Shape::Interpreter),
    ("python3", Shape::Interpreter),
    ("node", Shape::Interpreter),
];

fn literal(raw: &str) -> Option<String> {
    // Even quoted expansions are deliberately unknown. Backslashes can be
    // shell escapes or Windows separators; neither spelling is guessed here.
    if raw.contains(['$', '`', '*', '?', '[', '{', '\\']) || raw.starts_with('~') {
        return None;
    }
    let mut text = String::new();
    let mut quote = None;
    for ch in raw.chars() {
        match (quote, ch) {
            (None, '\'' | '"') => quote = Some(ch),
            (Some(open), close) if open == close => quote = None,
            _ => text.push(ch),
        }
    }
    quote.is_none().then_some(text)
}

fn resolve(text: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    if text.is_empty() || text == "-" || text.starts_with("/dev/") {
        return None;
    }
    let path = Path::new(text);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd?.join(path)
    };
    Some(super::resolve_with_existing_ancestors(&path))
}

fn operands<'a>(args: &'a [String], takes_value: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut skip = false;
    let mut options = true;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if options && arg == "--" {
            options = false;
            continue;
        }
        if options && arg.starts_with('-') {
            skip = takes_value.contains(&arg.as_str());
        } else {
            out.push(arg.as_str());
        }
    }
    out
}

fn redirected(source: &str, node: Node<'_>, out: &mut Vec<String>) {
    if node.kind() == "file_redirect" {
        let mut cursor = node.walk();
        let writes = node
            .children(&mut cursor)
            .any(|child| matches!(child.kind(), ">" | ">>" | "&>" | "&>>" | ">|"));
        if writes {
            if let Some(target) = node.child_by_field_name("destination").or_else(|| {
                let mut cursor = node.walk();
                node.named_children(&mut cursor).last()
            }) {
                if let Some(path) = literal(target.utf8_text(source.as_bytes()).unwrap_or("")) {
                    out.push(path);
                }
            }
        }
        return;
    }
    // Heredoc bodies are code/data, never shell redirect destinations.
    if node.kind() == "heredoc_redirect" {
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "command" {
            redirected(source, child, out);
        }
    }
}

fn heredoc(source: &str, node: Node<'_>) -> Option<String> {
    if node.kind() == "heredoc_body" {
        return Some(node.utf8_text(source.as_bytes()).ok()?.to_string());
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find_map(|child| heredoc(source, child))
}

fn patch_paths(text: &str, strip: usize) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let path = line
                .strip_prefix("+++ ")
                .or_else(|| line.strip_prefix("--- "))?;
            let path = path.split('\t').next()?;
            if path == "/dev/null" || path.contains(char::is_whitespace) {
                return None;
            }
            let path = literal(path)?;
            let parts: Vec<_> = Path::new(&path).components().skip(strip).collect();
            (!parts.is_empty()).then(|| {
                parts
                    .iter()
                    .collect::<PathBuf>()
                    .to_string_lossy()
                    .into_owned()
            })
        })
        .collect()
}

fn patch_input(
    args: &[String],
    source: &str,
    node: Node<'_>,
    cwd: Option<&Path>,
    git: bool,
) -> Vec<String> {
    if args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--check" | "--stat" | "--numstat" | "--summary" | "--dry-run"
        )
    }) {
        return Vec::new();
    }
    let mut strip = usize::from(git);
    for (i, arg) in args.iter().enumerate() {
        if let Some(value) = arg.strip_prefix("-p").filter(|v| !v.is_empty()) {
            let Ok(value) = value.parse() else {
                return Vec::new();
            };
            strip = value;
        } else if arg == "-p" {
            let Some(value) = args.get(i + 1).and_then(|v| v.parse().ok()) else {
                return Vec::new();
            };
            strip = value;
        }
        if matches!(arg.as_str(), "-d" | "--directory") || arg.starts_with("--directory=") {
            return Vec::new();
        }
    }
    let mut text = heredoc(source, node);
    if text.is_none() {
        let input = if git {
            operands(args, &["-p", "--include", "--exclude"])
                .first()
                .copied()
        } else {
            args.windows(2)
                .find(|pair| matches!(pair[0].as_str(), "-i" | "--input"))
                .map(|pair| pair[1].as_str())
        };
        if let Some(path) = input.and_then(|p| resolve(p, cwd)) {
            if std::fs::metadata(&path).is_ok_and(|m| m.len() <= 1024 * 1024) {
                text = std::fs::read_to_string(path).ok();
            }
        }
    }
    text.map(|text| patch_paths(&text, strip))
        .unwrap_or_default()
}

fn inline_paths(code: &str, javascript: bool) -> Vec<String> {
    fn text<'a>(node: Node<'_>, code: &'a str) -> &'a str {
        node.utf8_text(code.as_bytes()).unwrap_or("")
    }
    fn string(node: Node<'_>, code: &str) -> Option<String> {
        let raw = text(node, code);
        (node.kind() == "string" && (raw.starts_with('\'') || raw.starts_with('"')))
            .then(|| literal(raw))
            .flatten()
    }
    fn path(node: Node<'_>, code: &str, bindings: &HashMap<String, String>) -> Option<String> {
        if node.kind() == "identifier" {
            return bindings.get(text(node, code)).cloned();
        }
        if let Some(value) = string(node, code) {
            return Some(value);
        }
        if node.kind() == "call" {
            let name = text(node.child_by_field_name("function")?, code);
            if matches!(name, "Path" | "pathlib.Path") {
                return string(node.child_by_field_name("arguments")?.named_child(0)?, code);
            }
        }
        None
    }
    fn visit(
        node: Node<'_>,
        code: &str,
        bindings: &mut HashMap<String, String>,
        out: &mut Vec<String>,
    ) {
        if matches!(
            node.kind(),
            "function_definition"
                | "function_declaration"
                | "arrow_function"
                | "lambda"
                | "class_definition"
        ) {
            return;
        }
        if matches!(
            node.kind(),
            "assignment" | "variable_declarator" | "assignment_expression"
        ) {
            let left = node
                .child_by_field_name("left")
                .or_else(|| node.child_by_field_name("name"));
            let right = node
                .child_by_field_name("right")
                .or_else(|| node.child_by_field_name("value"));
            if let Some(left) = left.filter(|left| left.kind() == "identifier") {
                let name = text(left, code).to_string();
                // Only straight-line, top-level assignments establish literals.
                let mut parent = node.parent();
                let mut straight_line = true;
                while let Some(ancestor) = parent {
                    if !matches!(
                        ancestor.kind(),
                        "module"
                            | "program"
                            | "expression_statement"
                            | "lexical_declaration"
                            | "variable_declaration"
                    ) {
                        straight_line = false;
                    }
                    parent = ancestor.parent();
                }
                let value = right
                    .filter(|_| straight_line)
                    .and_then(|right| path(right, code, bindings));
                bindings.remove(&name);
                if let Some(value) = value {
                    bindings.insert(name, value);
                }
            }
        }
        if matches!(node.kind(), "call" | "call_expression") {
            if let (Some(function), Some(arguments)) = (
                node.child_by_field_name("function"),
                node.child_by_field_name("arguments"),
            ) {
                let name = text(function, code);
                let first = arguments.named_child(0);
                let target = if name == "open"
                    && arguments
                        .named_child(1)
                        .and_then(|n| string(n, code))
                        .is_some_and(|mode| mode.starts_with(['w', 'a']))
                {
                    first.and_then(|node| path(node, code, bindings))
                } else if name == "writeFileSync" || name.ends_with(".writeFileSync") {
                    first.and_then(|node| path(node, code, bindings))
                } else if name.ends_with(".write_text") {
                    function
                        .child_by_field_name("object")
                        .and_then(|object| path(object, code, bindings))
                } else {
                    None
                };
                if let Some(target) = target {
                    out.push(target);
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            visit(child, code, bindings, out);
        }
    }
    let mut parser = Parser::new();
    let language = if javascript {
        tree_sitter_javascript::LANGUAGE.into()
    } else {
        tree_sitter_python::LANGUAGE.into()
    };
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(code, None) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    visit(tree.root_node(), code, &mut HashMap::new(), &mut paths);
    paths
}

fn command_writes(source: &str, node: Node<'_>, cwd: Option<&Path>) -> Vec<PathBuf> {
    let mut wrapper = node;
    while let Some(parent) = wrapper.parent() {
        if matches!(parent.kind(), "redirected_statement" | "list" | "pipeline") {
            let mut cursor = parent.walk();
            if parent.kind() != "redirected_statement"
                && parent
                    .named_children(&mut cursor)
                    .last()
                    .is_none_or(|last| last.id() != wrapper.id())
            {
                break;
            }
            wrapper = parent;
        } else {
            break;
        }
    }
    let mut paths = Vec::new();
    redirected(source, wrapper, &mut paths);
    let parts = command_parts(source, node);
    // Keep unknown operands in position so they cannot turn an input into a
    // destination; the sentinel is discarded by literal() before resolution.
    let words: Vec<_> = parts
        .iter()
        .map(|part| literal(&part.text).unwrap_or_else(|| "$unknown".into()))
        .collect();
    let Some(head) = words
        .first()
        .and_then(|w| Path::new(w).file_name())
        .and_then(|w| w.to_str())
    else {
        return paths.iter().filter_map(|p| resolve(p, cwd)).collect();
    };
    let args = &words[1..];
    let shape = SHAPES
        .iter()
        .find(|(name, _)| *name == head)
        .map(|(_, shape)| *shape);
    let mut targets: Vec<String> = match shape {
        Some(Shape::Tee) => operands(args, &[]).into_iter().map(str::to_owned).collect(),
        Some(Shape::Remove) => operands(
            args,
            if head == "truncate" {
                &["-s", "--size", "-r", "--reference"]
            } else {
                &[]
            },
        )
        .into_iter()
        .map(str::to_owned)
        .collect(),
        Some(Shape::Touch) => operands(args, &["-t", "-d", "-r", "--date", "--reference"])
            .into_iter()
            .filter(|p| resolve(p, cwd).is_some_and(|p| !p.exists()))
            .map(str::to_owned)
            .collect(),
        Some(Shape::Destination) => {
            let target_dir = args
                .windows(2)
                .find(|pair| matches!(pair[0].as_str(), "-t" | "--target-directory"))
                .map(|pair| pair[1].as_str())
                .or_else(|| {
                    args.iter()
                        .find_map(|a| a.strip_prefix("--target-directory="))
                });
            let operands = operands(args, &["-m", "-o", "-g", "-t", "--target-directory"]);
            target_dir
                .or_else(|| (operands.len() >= 2).then(|| *operands.last().unwrap()))
                .into_iter()
                .map(str::to_owned)
                .collect()
        }
        Some(Shape::InPlace)
            if args.iter().any(|a| {
                a == "--in-place"
                    || a.starts_with("--in-place=")
                    || a.starts_with("-i")
                    || (head != "sed"
                        && a.starts_with('-')
                        && !a.starts_with("--")
                        && a.contains('i'))
            }) =>
        {
            let mut files = operands(args, &["-e", "--expression", "-f", "--file"]);
            if head == "sed" && files.first() == Some(&"") {
                files.remove(0);
            }
            if head == "sed"
                && !args
                    .iter()
                    .any(|a| a == "-e" || a.starts_with("-e") || a == "-f")
                && !files.is_empty()
            {
                files.remove(0);
            }
            // BSD sed's separate empty backup suffix is not a file operand.
            files
                .into_iter()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        }
        Some(Shape::Patch) => patch_input(args, source, wrapper, cwd, false),
        Some(Shape::Git) => match args.first().map(String::as_str) {
            Some("apply") => patch_input(&args[1..], source, wrapper, cwd, true),
            Some("checkout") => args
                .iter()
                .position(|a| a == "--")
                .map(|i| args[i + 1..].to_vec())
                .unwrap_or_default(),
            Some("restore") => operands(&args[1..], &["-s", "--source"])
                .into_iter()
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        },
        Some(Shape::Formatter) => {
            let check = args
                .iter()
                .any(|a| matches!(a.as_str(), "--check" | "--check-only"));
            let writes = !check
                && match head {
                    "cargo" => args.first().is_some_and(|a| a == "fmt"),
                    "rustfmt" | "black" => true,
                    "prettier" => args.iter().any(|a| a == "--write"),
                    "biome" => {
                        args.first()
                            .is_some_and(|a| matches!(a.as_str(), "format" | "check"))
                            && args.iter().any(|a| a == "--write")
                    }
                    "gofmt" => args.iter().any(|a| a == "-w"),
                    "ruff" => args.first().is_some_and(|a| a == "format"),
                    _ => false,
                };
            if !writes {
                Vec::new()
            } else if head == "cargo" {
                args.windows(2)
                    .find(|pair| pair[0] == "--manifest-path")
                    .map(|pair| vec![pair[1].clone()])
                    .unwrap_or_else(|| vec![".".into()])
            } else {
                let start = usize::from(matches!(head, "biome" | "ruff"));
                let files = operands(
                    &args[start..],
                    &[
                        "--config",
                        "--config-path",
                        "--line-length",
                        "--target-version",
                        "--edition",
                    ],
                );
                if files.is_empty() && matches!(head, "biome" | "ruff") {
                    vec![".".into()]
                } else {
                    files.into_iter().map(str::to_owned).collect()
                }
            }
        }
        Some(Shape::Interpreter) => {
            let flag = if head == "node" { "-e" } else { "-c" };
            // Interpreter source is not a shell path, so keep its quotes.
            let code = parts
                .windows(2)
                .find(|pair| pair[0].text == flag)
                .map(|pair| {
                    let raw = pair[1].text.as_str();
                    if raw.len() >= 2
                        && (raw.starts_with('\'') && raw.ends_with('\'')
                            || raw.starts_with('"') && raw.ends_with('"'))
                    {
                        raw[1..raw.len() - 1].to_string()
                    } else {
                        String::new()
                    }
                })
                .or_else(|| heredoc(source, wrapper));
            code.map(|code| inline_paths(&code, head == "node"))
                .unwrap_or_default()
        }
        _ => Vec::new(),
    };
    paths.append(&mut targets);
    paths
        .iter()
        .filter_map(|p| literal(p).and_then(|p| resolve(&p, cwd)))
        .collect()
}

pub(crate) fn literal_write_targets(
    command: &str,
    cwd: &Path,
    known_directory: &dyn Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    walk_shell_commands(command, cwd, known_directory, &mut |node, cwd| {
        for path in command_writes(command, node, cwd) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    });
    paths
}
