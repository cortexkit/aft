use crate::compress::generic::{strip_ansi, GenericCompressor};
use crate::compress::listing_fold::{
    fold_consecutive_runs, shape_key_for_basename, FoldEntry, FOLD_THRESHOLD,
};
use crate::compress::{CompressionResult, Compressor};

pub struct TreeCompressor;

impl Compressor for TreeCompressor {
    fn matches(&self, command: &str) -> bool {
        command_tokens(command).any(|token| token == "tree")
    }

    fn compress_with_exit_code(
        &self,
        _command: &str,
        output: &str,
        exit_code: Option<i64>,
    ) -> CompressionResult {
        let stripped = strip_ansi(output);
        if matches!(exit_code, Some(code) if code != 0) {
            return GenericCompressor::compress_output(&stripped).into();
        }
        if stripped.trim().is_empty() {
            return CompressionResult::new(stripped);
        }

        match compress_tree_listing(&stripped) {
            Some(folded) => CompressionResult::new(folded),
            None => GenericCompressor::compress_output(&stripped).into(),
        }
    }
}

fn command_tokens(command: &str) -> impl Iterator<Item = String> + '_ {
    command
        .split_whitespace()
        .map(|token| token.trim_matches(|ch| matches!(ch, '\'' | '"')))
        .filter(|token| {
            !matches!(
                *token,
                "npx" | "pnpm" | "yarn" | "bun" | "bunx" | "exec" | "-m"
            )
        })
        .map(|token| {
            token
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(token)
                .trim_end_matches(".cmd")
                .to_string()
        })
}

fn compress_tree_listing(output: &str) -> Option<String> {
    let doc = parse_tree(output)?;
    let mut lines = render_tree(&doc);
    lines.extend(doc.trailer.iter().cloned());
    Some(lines.join("\n"))
}

#[derive(Debug)]
struct TreeDoc {
    nodes: Vec<TreeNode>,
    roots: Vec<usize>,
    trailer: Vec<String>,
}

#[derive(Debug)]
struct TreeNode {
    label: String,
    children: Vec<usize>,
}

#[derive(Debug)]
struct ParsedTreeEntry<'a> {
    depth: usize,
    label: &'a str,
}

fn parse_tree(output: &str) -> Option<TreeDoc> {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return Some(TreeDoc {
            nodes: Vec::new(),
            roots: Vec::new(),
            trailer: Vec::new(),
        });
    }

    let (tree_lines, trailer) = split_tree_trailer(&lines);
    if tree_lines.is_empty() {
        return None;
    }

    let mut nodes = Vec::new();
    let mut roots = Vec::new();
    let mut stack: Vec<usize> = Vec::new();

    for line in tree_lines {
        if line.trim().is_empty() {
            return None;
        }

        if let Some(entry) = parse_tree_entry(line) {
            if entry.depth == 0 || entry.label.trim().is_empty() || stack.len() < entry.depth {
                return None;
            }
            let parent = stack[entry.depth - 1];
            let id = push_node(&mut nodes, entry.label.to_string());
            nodes[parent].children.push(id);
            stack.truncate(entry.depth);
            stack.push(id);
            continue;
        }

        if looks_like_malformed_tree_line(line) {
            return None;
        }

        let id = push_node(&mut nodes, line.to_string());
        roots.push(id);
        stack.clear();
        stack.push(id);
    }

    if roots.is_empty() {
        return None;
    }

    Some(TreeDoc {
        nodes,
        roots,
        trailer,
    })
}

fn push_node(nodes: &mut Vec<TreeNode>, label: String) -> usize {
    let id = nodes.len();
    nodes.push(TreeNode {
        label,
        children: Vec::new(),
    });
    id
}

fn split_tree_trailer<'a>(lines: &'a [&'a str]) -> (&'a [&'a str], Vec<String>) {
    let Some(summary_index) = lines.iter().rposition(|line| !line.trim().is_empty()) else {
        return (lines, Vec::new());
    };

    if !is_tree_summary_line(lines[summary_index]) {
        return (lines, Vec::new());
    }

    let mut trailer_start = summary_index;
    while trailer_start > 0 && lines[trailer_start - 1].trim().is_empty() {
        trailer_start -= 1;
    }

    (
        &lines[..trailer_start],
        lines[trailer_start..]
            .iter()
            .map(|line| (*line).to_string())
            .collect(),
    )
}

fn is_tree_summary_line(line: &str) -> bool {
    let trimmed = line.trim();
    let Some((directories, files)) = trimmed.split_once(',') else {
        return false;
    };

    count_word(directories.trim(), "directory", "directories")
        && count_word(files.trim(), "file", "files")
}

fn count_word(text: &str, singular: &str, plural: &str) -> bool {
    let mut parts = text.split_whitespace();
    let Some(count) = parts.next() else {
        return false;
    };
    if count.is_empty() || !count.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    let Some(noun) = parts.next() else {
        return false;
    };
    (noun == singular || noun == plural) && parts.next().is_none()
}

fn parse_tree_entry(line: &str) -> Option<ParsedTreeEntry<'_>> {
    let mut consumed = 0usize;
    let mut ancestor_units = 0usize;

    while let Some(next) = consume_tree_indent_unit(line, consumed) {
        ancestor_units += 1;
        consumed = next;
    }

    let label_start = consume_tree_connector(line, consumed)?;
    Some(ParsedTreeEntry {
        depth: ancestor_units + 1,
        label: &line[label_start..],
    })
}

fn consume_tree_indent_unit(line: &str, start: usize) -> Option<usize> {
    let (first, _) = next_char(line, start)?;
    if is_tree_vertical_char(first) {
        let mut index = start + first.len_utf8();
        for _ in 0..3 {
            index = consume_tree_space_char(line, index)?;
        }
        Some(index)
    } else if is_tree_space_char(first) {
        let mut index = start;
        for _ in 0..4 {
            index = consume_tree_space_char(line, index)?;
        }
        Some(index)
    } else {
        None
    }
}

fn consume_tree_connector(line: &str, start: usize) -> Option<usize> {
    let (branch, mut index) = next_char(line, start)?;
    if !is_tree_connector_char(branch) {
        return None;
    }

    for _ in 0..2 {
        let (horizontal, next) = next_char(line, index)?;
        if !is_tree_horizontal_char(horizontal) {
            return None;
        }
        index = next;
    }

    consume_tree_space_char(line, index)
}

fn consume_tree_space_char(line: &str, start: usize) -> Option<usize> {
    let (ch, next) = next_char(line, start)?;
    is_tree_space_char(ch).then_some(next)
}

fn next_char(line: &str, start: usize) -> Option<(char, usize)> {
    let ch = line.get(start..)?.chars().next()?;
    Some((ch, start + ch.len_utf8()))
}

fn is_tree_space_char(ch: char) -> bool {
    matches!(ch, ' ' | '\u{00a0}')
}

fn is_tree_vertical_char(ch: char) -> bool {
    matches!(ch, '│' | '|')
}

fn is_tree_connector_char(ch: char) -> bool {
    matches!(ch, '├' | '└' | '|' | '`')
}

fn is_tree_horizontal_char(ch: char) -> bool {
    matches!(ch, '─' | '-')
}

fn looks_like_malformed_tree_line(line: &str) -> bool {
    let trimmed = line.trim_start_matches(|ch: char| ch.is_whitespace() || is_tree_space_char(ch));
    trimmed.starts_with(|ch: char| is_tree_vertical_char(ch) || matches!(ch, '├' | '└' | '`'))
}

#[derive(Debug, PartialEq, Eq)]
enum FoldedChild {
    Node(usize),
    Summary(String),
}

fn render_tree(doc: &TreeDoc) -> Vec<String> {
    let mut out = Vec::new();
    for &root in &doc.roots {
        let label = doc.nodes[root].label.clone();
        out.push(label.clone());
        render_children(&doc.nodes, root, "", &label, &mut out);
    }
    out
}

fn render_children(
    nodes: &[TreeNode],
    parent: usize,
    prefix: &str,
    parent_path: &str,
    out: &mut Vec<String>,
) {
    let children = fold_child_entries(nodes, &nodes[parent].children, parent_path);
    let child_count = children.len();

    for (index, child) in children.iter().enumerate() {
        let is_last = index + 1 == child_count;
        let connector = if is_last { "└── " } else { "├── " };
        match child {
            FoldedChild::Summary(label) => {
                out.push(format!("{prefix}{connector}{label}"));
            }
            FoldedChild::Node(id) => {
                let label = &nodes[*id].label;
                out.push(format!("{prefix}{connector}{label}"));
                if !nodes[*id].children.is_empty() {
                    let next_prefix =
                        format!("{}{}", prefix, if is_last { "    " } else { "│   " });
                    let child_path = join_tree_path(parent_path, label);
                    render_children(nodes, *id, &next_prefix, &child_path, out);
                }
            }
        }
    }
}

fn fold_child_entries(
    nodes: &[TreeNode],
    child_ids: &[usize],
    parent_path: &str,
) -> Vec<FoldedChild> {
    let mut out = Vec::new();
    let mut index = 0usize;

    while index < child_ids.len() {
        let first_id = child_ids[index];
        let foldable = nodes[first_id].children.is_empty();

        if !foldable {
            out.push(FoldedChild::Node(first_id));
            index += 1;
            continue;
        }

        let key = shape_key_for_basename(parent_path, &nodes[first_id].label);
        let mut end = index + 1;
        while end < child_ids.len() {
            let id = child_ids[end];
            if !nodes[id].children.is_empty()
                || shape_key_for_basename(parent_path, &nodes[id].label) != key
            {
                break;
            }
            end += 1;
        }

        if end - index >= FOLD_THRESHOLD {
            out.push(FoldedChild::Summary(fold_summary_for_run(
                nodes,
                &child_ids[index..end],
                parent_path,
            )));
        } else {
            out.extend(child_ids[index..end].iter().copied().map(FoldedChild::Node));
        }
        index = end;
    }

    out
}

fn fold_summary_for_run(nodes: &[TreeNode], run: &[usize], parent_path: &str) -> String {
    let entries = run
        .iter()
        .map(|id| {
            let basename = nodes[*id].label.clone();
            FoldEntry {
                line: basename.clone(),
                dir: String::new(),
                shape_key: shape_key_for_basename(parent_path, &basename),
                basename,
            }
        })
        .collect();
    let folded = fold_consecutive_runs(entries);
    debug_assert_eq!(folded.len(), 1);
    folded.into_iter().next().unwrap_or_default()
}

fn join_tree_path(parent_path: &str, label: &str) -> String {
    if parent_path.is_empty() {
        label.to_string()
    } else {
        format!("{}/{}", parent_path.trim_end_matches('/'), label)
    }
}

#[derive(Clone, Copy)]
enum TreeFixtureFormat {
    Utf8BoxAsciiIndent,
    Utf8BoxNbspIndent,
    CLocaleAscii,
}

impl TreeFixtureFormat {
    fn continuing_indent(self) -> &'static str {
        match self {
            Self::Utf8BoxAsciiIndent => "\u{2502}   ",
            Self::Utf8BoxNbspIndent => "\u{2502}\u{00a0}\u{00a0} ",
            Self::CLocaleAscii => "|   ",
        }
    }

    fn blank_indent(self) -> &'static str {
        "    "
    }

    fn branch_connector(self) -> &'static str {
        match self {
            Self::Utf8BoxAsciiIndent | Self::Utf8BoxNbspIndent => "\u{251c}\u{2500}\u{2500} ",
            Self::CLocaleAscii => "|-- ",
        }
    }

    fn last_connector(self) -> &'static str {
        match self {
            Self::Utf8BoxAsciiIndent | Self::Utf8BoxNbspIndent => "\u{2514}\u{2500}\u{2500} ",
            Self::CLocaleAscii => "`-- ",
        }
    }
}

fn build_lebench_tree_fixture_with_format(format: TreeFixtureFormat) -> String {
    let mut lines = Vec::with_capacity(207);
    lines.push("src".to_string());
    lines.push(format!("{}generated", format.branch_connector()));
    lines.push(format!(
        "{}{}client",
        format.continuing_indent(),
        format.last_connector()
    ));
    let nested_prefix = format!("{}{}", format.continuing_indent(), format.blank_indent());
    for i in 0..=100u32 {
        lines.push(format!(
            "{nested_prefix}{}module_{i:03}.ts",
            format.branch_connector()
        ));
    }
    lines.push(format!(
        "{nested_prefix}{}module_100_NEEDLE_FILE_marker.ts",
        format.branch_connector()
    ));
    for i in 101..=198u32 {
        lines.push(format!(
            "{nested_prefix}{}module_{i:03}.ts",
            format.branch_connector()
        ));
    }
    lines.push(format!(
        "{nested_prefix}{}module_199.ts",
        format.last_connector()
    ));
    lines.push(format!("{}main.ts", format.last_connector()));
    lines.push(String::new());
    lines.push("2 directories, 202 files".to_string());
    lines.join("\n")
}

pub fn build_lebench_tree_fixture() -> String {
    build_lebench_tree_fixture_with_format(TreeFixtureFormat::Utf8BoxAsciiIndent)
}

pub fn build_lebench_tree_utf8_nbsp_fixture() -> String {
    build_lebench_tree_fixture_with_format(TreeFixtureFormat::Utf8BoxNbspIndent)
}

pub fn build_lebench_tree_c_locale_ascii_fixture() -> String {
    build_lebench_tree_fixture_with_format(TreeFixtureFormat::CLocaleAscii)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEEDLE: &str = "module_100_NEEDLE_FILE_marker.ts";

    fn assert_tree_fixture_folds(input: &str) -> String {
        let line_count = input.lines().count();
        let out = compress_tree_listing(input).expect("tree parses");

        assert!(out.contains(NEEDLE), "needle must survive; got:\n{out}");
        assert!(
            out.contains("module_*.ts —"),
            "homogeneous module run should fold: {out}"
        );
        assert!(
            out.contains("2 directories, 202 files"),
            "tree summary trailer should be preserved: {out}"
        );
        assert!(
            out.lines().count() < line_count / 2,
            "should compress dramatically: {line_count} -> {} lines",
            out.lines().count()
        );
        out
    }

    #[test]
    fn matches_tree_invocations() {
        let c = TreeCompressor;
        assert!(c.matches("tree"));
        assert!(c.matches("tree -a src"));
        assert!(c.matches("cd /tmp && tree ."));
        assert!(!c.matches("treeify"));
    }

    #[test]
    fn lebench_tree_folds_sibling_run_and_preserves_needle_and_trailer() {
        let input = build_lebench_tree_fixture();
        let out = assert_tree_fixture_folds(&input);

        assert!(
            out.contains("│       ├── module_100_NEEDLE_FILE_marker.ts"),
            "needle should keep its sibling-tree prefix: {out}"
        );
    }

    #[test]
    fn lebench_utf8_nbsp_tree_folds_sibling_run_and_preserves_needle_and_trailer() {
        let input = build_lebench_tree_utf8_nbsp_fixture();
        assert!(
            input.contains("\u{2502}\u{00a0}\u{00a0}     \u{251c}\u{2500}\u{2500} module_000.ts")
        );

        let out = assert_tree_fixture_folds(&input);

        assert!(
            out.contains("│       ├── module_100_NEEDLE_FILE_marker.ts"),
            "needle should keep its sibling-tree prefix after normalization: {out}"
        );
    }

    #[test]
    fn lebench_c_locale_ascii_tree_folds_sibling_run_and_preserves_needle_and_trailer() {
        let input = build_lebench_tree_c_locale_ascii_fixture();
        assert!(input.contains("|       |-- module_000.ts"));

        assert_tree_fixture_folds(&input);
    }

    #[test]
    fn tree_compression_is_deterministic_across_fixture_formats() {
        for input in [
            build_lebench_tree_fixture(),
            build_lebench_tree_utf8_nbsp_fixture(),
            build_lebench_tree_c_locale_ascii_fixture(),
        ] {
            let first = compress_tree_listing(&input).expect("tree parses");
            let second = compress_tree_listing(&input).expect("tree parses");
            let recompressed = compress_tree_listing(&first).expect("compressed tree parses");
            assert_eq!(first, second);
            assert_eq!(first, recompressed);
        }
    }

    #[test]
    fn nested_directories_fold_independent_sibling_groups() {
        let mut lines = vec![".".to_string(), "├── app".to_string()];
        for i in 0..10u32 {
            lines.push(format!("│   ├── module_{i:03}.rs"));
        }
        lines.push("│   └── special.rs".to_string());
        lines.push("└── lib".to_string());
        for i in 0..8u32 {
            lines.push(format!("    ├── component_{i:03}.rs"));
        }
        lines.push("    └── keep.rs".to_string());
        lines.push(String::new());
        lines.push("2 directories, 20 files".to_string());
        let input = lines.join("\n");

        let out = compress_tree_listing(&input).expect("tree parses");

        assert!(out.contains("│   ├── module_*.rs — 10 files"), "{out}");
        assert!(out.contains("│   └── special.rs"), "{out}");
        assert!(out.contains("    ├── component_*.rs — 8 files"), "{out}");
        assert!(out.contains("    └── keep.rs"), "{out}");
        assert!(out.contains("2 directories, 20 files"), "{out}");
    }

    #[test]
    fn mixed_ascii_and_nbsp_indent_space_chars_still_parse() {
        let lines = [
            ".".to_string(),
            "├── src".to_string(),
            format!("\u{2502} \u{00a0} ├── module_{:03}.rs", 0),
            format!("\u{2502}\u{00a0}  ├── module_{:03}.rs", 1),
            format!("\u{2502}  \u{00a0}├── module_{:03}.rs", 2),
            format!("\u{2502}\u{00a0}\u{00a0} ├── module_{:03}.rs", 3),
            format!("\u{2502} \u{00a0} ├── module_{:03}.rs", 4),
            format!("\u{2502}\u{00a0}  ├── module_{:03}.rs", 5),
            format!("\u{2502}  \u{00a0}├── module_{:03}.rs", 6),
            format!("\u{2502}\u{00a0}\u{00a0} ├── module_{:03}.rs", 7),
            "\u{2502} \u{00a0} \u{2514}\u{2500}\u{2500} keep.rs".to_string(),
            "└── tail.rs".to_string(),
            String::new(),
            "1 directory, 10 files".to_string(),
        ]
        .join("\n");

        let out = compress_tree_listing(&lines).expect("tree parses");

        assert!(out.contains("│   ├── module_*.rs — 8 files"), "{out}");
        assert!(out.contains("│   └── keep.rs"), "{out}");
        assert!(out.contains("└── tail.rs"), "{out}");
        assert!(out.contains("1 directory, 10 files"), "{out}");
    }

    #[test]
    fn small_sibling_groups_pass_through_unchanged() {
        let mut lines = vec![".".to_string()];
        for i in 0..6u32 {
            lines.push(format!("├── file_{i}.txt"));
        }
        lines.push("└── file_6.txt".to_string());
        lines.push(String::new());
        lines.push("0 directories, 7 files".to_string());
        let input = lines.join("\n");

        let out = compress_tree_listing(&input).expect("tree parses");

        assert_eq!(out, input);
    }

    #[test]
    fn non_tree_shaped_output_degrades_to_generic_passthrough() {
        let c = TreeCompressor;
        let out = c.compress_with_exit_code(
            "tree --bad-option",
            "tree: invalid option -- z\nusage: tree [options]",
            Some(1),
        );
        assert!(out.text.contains("tree: invalid option"));
        assert!(out.text.contains("usage: tree"));
    }
}
