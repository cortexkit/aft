//! Python block indentation that tree-sitter-python accepts but CPython rejects.
//!
//! tree-sitter-python's external scanner emits an INDENT token only where the
//! grammar expects one, right after a `:` that opens a block. A line indented
//! deeper anywhere else parses as one more statement of the current block, a
//! dedent to a level no enclosing block uses parses as a statement of the
//! outer block, and a header followed directly by a dedent parses as an empty
//! block. CPython rejects all three ("unexpected indent", "unindent does not
//! match any outer indentation level", "expected an indented block"), but the
//! tree holds no ERROR or MISSING node, so `has_error()` alone accepts them and
//! an edit that introduces one would be reported as valid. This module checks
//! the parsed tree for those shapes.

use tree_sitter::{Node, Tree};

/// Leading whitespace measured the way CPython's tokenizer measures it: the
/// column with tab stops every 8, and the column with every tab counted as 1.
/// CPython treats two lines as the same level only when both columns agree,
/// and raises TabError when they disagree, so comparing both reproduces its
/// IndentationError and TabError verdicts without a tab-width policy of our
/// own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Indent {
    tab8: usize,
    tab1: usize,
}

impl Indent {
    const ZERO: Indent = Indent { tab8: 0, tab1: 0 };

    fn deeper_than(self, other: Indent) -> bool {
        self.tab8 > other.tab8 && self.tab1 > other.tab1
    }
}

/// Measure a line prefix that should hold only indentation. `None` when it
/// holds anything else, meaning the position is not at the start of a line.
fn measure(prefix: &str) -> Option<Indent> {
    let mut indent = Indent::ZERO;
    for ch in prefix.chars() {
        match ch {
            ' ' => {
                indent.tab8 += 1;
                indent.tab1 += 1;
            }
            '\t' => {
                indent.tab8 = (indent.tab8 / 8 + 1) * 8;
                indent.tab1 += 1;
            }
            // CPython resets the column at a form feed.
            '\x0c' => indent = Indent::ZERO,
            _ => return None,
        }
    }
    Some(indent)
}

/// The indentation of the line holding `byte`, when `byte` is the first
/// non-whitespace character of a line that does not continue the previous
/// line through a backslash. `None` otherwise: a statement after `;` or on
/// its header's line, and a statement on a continuation line, have no
/// indentation of their own to compare. Returning `None` only ever skips a
/// comparison, so an unusual layout can hide an error but never invent one.
///
/// A backslash continues a line only as a `line_continuation` token in
/// code. One that ends a comment is part of the comment (Python never
/// continues comments), so the next line is still checked.
fn line_start_indent(root: Node<'_>, source: &str, byte: usize) -> Option<Indent> {
    let line_start = source[..byte].rfind('\n').map_or(0, |at| at + 1);
    let indent = measure(&source[line_start..byte])?;
    if line_start > 0 {
        let previous = source[..line_start - 1].trim_end_matches('\r');
        if previous.ends_with('\\') {
            let backslash = previous.len() - 1;
            let mut node = root.descendant_for_byte_range(backslash, backslash + 1);
            while let Some(current) = node {
                if current.kind() == "line_continuation" {
                    return None;
                }
                node = current.parent();
            }
        }
    }
    Some(indent)
}

fn line_of(node: Node<'_>) -> usize {
    node.start_position().row + 1
}

/// Check one `block` (or the `module`) and return the 1-based line of the
/// first problem in it, if any. Nested blocks are checked separately.
fn check_suite(node: Node<'_>, root: Node<'_>, source: &str) -> Option<usize> {
    let is_module = node.kind() == "module";
    let mut walker = node.walk();
    let mut first_problem: Option<usize> = None;
    let mut note = |line: usize| {
        first_problem = Some(first_problem.map_or(line, |seen| seen.min(line)));
    };
    let mut statements = 0usize;
    // Module statements must start at column 0; a block takes its level from
    // its first statement that starts a line.
    let mut level = is_module.then_some(Indent::ZERO);
    for statement in node
        .named_children(&mut walker)
        .filter(|child| child.kind() != "comment")
    {
        statements += 1;
        let Some(indent) = line_start_indent(root, source, statement.start_byte()) else {
            continue;
        };
        match level {
            None => level = Some(indent),
            // Every statement of one suite must be at the same level.
            Some(expected) if expected != indent => note(line_of(statement)),
            Some(_) => {}
        }
    }
    if is_module {
        return first_problem;
    }
    // Python requires at least one statement in a block; comments do not count.
    if statements == 0 {
        note(line_of(node));
    }
    // A block on a later line than its header must be indented deeper than
    // the header's line.
    if let (Some(level), Some(header)) = (level, node.parent()) {
        if node.start_position().row > header.start_position().row {
            if let Some(header_indent) = line_start_indent(root, source, header.start_byte()) {
                if !level.deeper_than(header_indent) {
                    note(line_of(node));
                }
            }
        }
    }
    first_problem
}

/// 1-based line of the first block-indentation error in a Python tree, or
/// `None` when every block is indented the way CPython requires. Call it only
/// on a tree without ERROR or MISSING nodes; error recovery can attach
/// statements to arbitrary blocks.
pub(crate) fn first_indentation_error_line(tree: &Tree, source: &str) -> Option<usize> {
    let mut first: Option<usize> = None;
    let mut cursor = tree.root_node().walk();
    // Iterative pre-order walk: deeply nested code must not overflow a
    // recursive one.
    loop {
        let node = cursor.node();
        if matches!(node.kind(), "block" | "module") {
            if let Some(line) = check_suite(node, tree.root_node(), source) {
                first = Some(first.map_or(line, |seen| seen.min(line)));
            }
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return first;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        parser.parse(source, None).unwrap()
    }

    /// The error line for `source`, asserting first that tree-sitter itself
    /// accepts it, so each case exercises this module and not `has_error()`.
    fn error_line(source: &str) -> Option<usize> {
        let tree = parse(source);
        assert!(
            !tree.root_node().has_error(),
            "tree-sitter rejects {source:?}"
        );
        first_indentation_error_line(&tree, source)
    }

    #[test]
    fn flags_indentation_errors_tree_sitter_accepts() {
        for (label, source, line) in [
            // An `if` whose body was dedented to the header's level.
            (
                "expected_indented_block",
                "def f(x):\n    if x:\n    a = 10\n    b = 20\n    return x\n",
                2,
            ),
            (
                "unexpected_indent",
                "def g():\n    if True:\n        return 2\n            y = 5\n        z = 6\n    return 3\n",
                4,
            ),
            (
                "dedent_between_levels",
                "def h():\n    if x:\n        a = 1\n      b = 2\n",
                4,
            ),
            (
                "comment_only_body",
                "def k():\n    if x:\n        # todo\n    return 1\n",
                2,
            ),
            ("indented_module_statement", "a = 1\n  b = 2\n", 2),
            // Python never continues a comment, even one ending in `\`.
            (
                "comment_ending_in_backslash",
                "a = 1  # note \\\n  x = 3\n",
                2,
            ),
            // Same tab8 column, different tab1 column: CPython's TabError.
            (
                "inconsistent_tabs_and_spaces",
                "if x:\n\ta = 1\n        b = 2\n",
                3,
            ),
        ] {
            assert_eq!(error_line(source), Some(line), "{label}");
        }
    }

    #[test]
    fn accepts_valid_layouts() {
        for (label, source) in [
            ("nested", "def f(x):\n    if x:\n        a = 1\n    return x\n"),
            ("same_line_suite", "if x: a = 1; b = 2\nelse: pass\n"),
            (
                "comments_at_any_column",
                "def f():\n    a = 1\n# note\n        # note\n    return a\n",
            ),
            (
                "backslash_continuation",
                "def f():\n    a = 1 + \\\n        2\n    return a\n",
            ),
            ("tabs", "def f():\n\tif x:\n\t\treturn 1\n\treturn 2\n"),
            // A real continuation joins the lines, so a statement that
            // starts the continued line has no indentation of its own.
            ("statement_after_continuation", "x = 1; \\\n  y = 2\n"),
            // A comment ending in a backslash, followed by a correctly
            // indented line.
            ("comment_ending_in_backslash_valid", "a = 1  # note \\\nb = 2\n"),
            (
                "bracket_continuation",
                "def f():\n    a = [\n  1,\n            2,\n]\n    return a\n",
            ),
            (
                "multiline_string",
                "def f():\n    s = \"\"\"\nno indent here\n        deep\n\"\"\"\n    return s\n",
            ),
            (
                "decorated_class",
                "@dataclass\nclass A:\n    x: int = 0\n\n    @property\n    def y(self):\n        return self.x\n",
            ),
            (
                "match_statement",
                "match p:\n    case 1:\n        pass\n    case _:\n        pass\n",
            ),
            (
                "try_chain",
                "try:\n    a()\nexcept E as e:\n    b()\nelse:\n    c()\nfinally:\n    d()\n",
            ),
            ("form_feed", "def f():\n    a = 1\n\x0c    return a\n"),
            ("crlf", "def f():\r\n    if x:\r\n        a = 1\r\n    return a\r\n"),
            ("empty_file", ""),
        ] {
            assert_eq!(error_line(source), None, "{label} falsely flagged");
        }
    }

    /// Every Python file in this repository (benchmark fixtures, scripts and
    /// test fixtures) is valid Python, so the check must accept each one that
    /// tree-sitter accepts. This is the bounded, always-on form of the larger
    /// corpus measurement in the integration tests.
    #[test]
    fn accepts_every_python_file_in_the_repository() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut pending = vec![root];
        let mut checked = 0usize;
        let mut flagged = Vec::new();
        while let Some(dir) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    // Skip VCS metadata, build output, installed packages and
                    // virtual environments: none of them is repository source.
                    if !name.starts_with('.')
                        && !matches!(
                            name.as_ref(),
                            "target" | "node_modules" | "__pycache__" | "venv" | "dist"
                        )
                    {
                        pending.push(path);
                    }
                } else if kind.is_file() && name.ends_with(".py") {
                    let Ok(source) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    let tree = parse(&source);
                    if tree.root_node().has_error() {
                        continue;
                    }
                    checked += 1;
                    if let Some(line) = first_indentation_error_line(&tree, &source) {
                        flagged.push(format!("{}:{line}", path.display()));
                    }
                }
            }
        }
        assert!(checked >= 50, "found only {checked} Python files");
        assert!(flagged.is_empty(), "falsely flagged: {flagged:?}");
    }
}
