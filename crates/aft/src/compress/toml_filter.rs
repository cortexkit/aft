//! Declarative TOML output filters for hoisted bash compression.
//!
//! TOML filters are a complement to the Rust `Compressor` modules. They cover
//! the long tail of CLI tools whose output is amenable to simple
//! strip + truncate + cap + shortcircuit pipelines without requiring stateful
//! parsing or invocation rewrite.
//!
//! ## Pipeline
//!
//! For a matched filter, output flows through:
//! 1. `[strip]` — drop lines matching any regex (compiled with multiline mode)
//! 2. `[shortcircuit]` — if remaining content matches `when`, replace with `replacement`
//!    (compiled without multiline mode; use `(?m)` explicitly for line anchors,
//!    and use `\A...\z` for full-body anchors such as empty output)
//! 3. `[truncate]` — middle-truncate lines longer than `line_max`
//! 4. `[cap]` — keep at most `max_lines` lines (head/tail/middle)
//!
//! ## Sources
//!
//! Filters come from three sources, layered project > user > builtin by filename:
//! - **builtin**: shipped via `include_str!()` from `compress/builtin_filters/`
//! - **user**: `~/.config/aft/filters/*.toml` (or `$XDG_CONFIG_HOME`-aware path)
//! - **project**: `<project>/.aft/filters/*.toml` — trust-gated, see [`crate::compress::trust`]
//!
//! Bad filters are skipped with a warning, never panic.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use regex::{Regex, RegexBuilder};
use serde::Deserialize;

/// Approximate per-regex byte budget. Matches the budget RTK uses for its
/// declarative filters; far more than any realistic compress regex needs.
const REGEX_SIZE_LIMIT: usize = 2 * 1024 * 1024;

/// Hard ceiling on a single filter's combined regex set. Prevents pathologically
/// large filter files from inflating startup cost or memory.
const MAX_PATTERNS_PER_FILTER: usize = 256;

/// Default per-line truncation when `[truncate]` is omitted entirely. Matches
/// existing AFT generic compressor behavior of "tolerate long lines unless told
/// otherwise".
const DEFAULT_LINE_MAX: usize = usize::MAX;

/// Default line cap when `[cap]` is omitted. Matches the inline-cap budget.
const DEFAULT_MAX_LINES: usize = usize::MAX;

/// One TOML filter, parsed and ready to apply.
#[derive(Debug, Clone)]
pub struct TomlFilter {
    pub name: String,
    pub source: FilterSource,
    pub matches: Vec<String>,
    pub description: Option<String>,
    pub strip: Vec<Regex>,
    pub line_max: usize,
    pub max_lines: usize,
    pub keep: KeepMode,
    pub shortcircuit_when: Option<Regex>,
    pub shortcircuit_replacement: Option<String>,
    pub strip_ansi: bool,
}

/// Where a filter came from. Drives priority and trust handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterSource {
    Builtin,
    User { path: PathBuf },
    Project { path: PathBuf },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeepMode {
    Head,
    #[default]
    Tail,
    Middle,
}

/// Aggregate registry of all loaded filters across all sources.
///
/// Lookup is by command program name (first non-env, non-path token of the
/// command). Project filters override user filters override builtin filters
/// when their `matches[]` overlap.
#[derive(Debug, Default, Clone)]
pub struct FilterRegistry {
    /// Map from program name → resolved filter (already merged across sources).
    by_match: HashMap<String, TomlFilter>,
    /// All filters, indexed by `(source-priority, name)` for tooling/listing.
    /// Order is builtin → user → project so lower-priority entries appear first.
    all: Vec<TomlFilter>,
    /// Non-fatal load warnings the agent or doctor command should surface.
    warnings: Vec<String>,
}

impl FilterRegistry {
    /// Look up a filter for a command. Returns the highest-priority filter
    /// whose `matches[]` contains the command's program name.
    pub fn lookup(&self, command: &str) -> Option<&TomlFilter> {
        let program = program_name(command)?;
        self.by_match.get(program)
    }

    /// All filters loaded into this registry, in builtin → user → project order.
    pub fn all(&self) -> &[TomlFilter] {
        &self.all
    }

    /// Non-fatal warnings emitted during load. Use these for doctor / configure
    /// warning surfacing.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// Build a registry from sources in priority order.
///
/// `builtin_inputs` is supplied by the caller (shipped via `include_str!`)
/// because constants live in `crate::compress::mod`.
pub fn build_registry(
    builtin_inputs: &[(&'static str, &'static str)],
    user_dir: Option<&Path>,
    project_dir: Option<&Path>,
) -> FilterRegistry {
    let mut registry = FilterRegistry::default();

    // Builtin: always loaded.
    for (name, content) in builtin_inputs {
        match parse_filter(name, content, FilterSource::Builtin) {
            Ok(filter) => insert_filter(&mut registry, filter),
            Err(e) => registry
                .warnings
                .push(format!("builtin filter {name}: {e}")),
        }
    }

    // User: loaded if dir exists.
    if let Some(dir) = user_dir {
        load_dir(dir, &mut registry, |path| FilterSource::User {
            path: path.to_path_buf(),
        });
    }

    // Project: loaded if dir exists. Caller is responsible for trust gating
    // *before* calling this — pass `None` for `project_dir` if the project
    // is untrusted.
    if let Some(dir) = project_dir {
        load_dir(dir, &mut registry, |path| FilterSource::Project {
            path: path.to_path_buf(),
        });
    }

    registry
}

fn load_dir<F>(dir: &Path, registry: &mut FilterRegistry, source_for: F)
where
    F: Fn(&Path) -> FilterSource,
{
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            // Missing dir is normal; only warn on real IO errors.
            if e.kind() != std::io::ErrorKind::NotFound {
                registry
                    .warnings
                    .push(format!("filter dir {}: {e}", dir.display()));
            }
            return;
        }
    };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(|res| res.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("toml"))
        .collect();
    paths.sort();

    for path in paths {
        let content = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                registry
                    .warnings
                    .push(format!("filter {}: read failed: {e}", path.display()));
                continue;
            }
        };
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let source = source_for(&path);
        match parse_filter(&name, &content, source) {
            Ok(filter) => insert_filter(registry, filter),
            Err(e) => registry
                .warnings
                .push(format!("filter {}: {e}", path.display())),
        }
    }
}

fn insert_filter(registry: &mut FilterRegistry, filter: TomlFilter) {
    // Higher-priority sources (project > user > builtin) overwrite earlier
    // entries with the same `match` keyword. Filename-keyed override is also
    // implicit because higher-priority filters arrive later in build order.
    for keyword in &filter.matches {
        registry.by_match.insert(keyword.clone(), filter.clone());
    }
    // Replace any existing entry in `all` for the same logical name+source so
    // re-loads don't duplicate (mainly relevant in tests).
    registry
        .all
        .retain(|existing| !(existing.name == filter.name && existing.source == filter.source));
    registry.all.push(filter);
}

#[derive(Debug, Deserialize)]
struct RawFilter {
    #[serde(default)]
    filter: RawFilterMeta,
    #[serde(default)]
    strip: Option<RawStrip>,
    #[serde(default)]
    truncate: Option<RawTruncate>,
    #[serde(default)]
    cap: Option<RawCap>,
    #[serde(default)]
    shortcircuit: Option<RawShortcircuit>,
    #[serde(default)]
    ansi: Option<RawAnsi>,
}

#[derive(Debug, Deserialize, Default)]
struct RawFilterMeta {
    #[serde(default)]
    matches: Vec<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawStrip {
    #[serde(default)]
    patterns: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawTruncate {
    #[serde(default)]
    line_max: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
struct RawCap {
    #[serde(default)]
    max_lines: Option<usize>,
    #[serde(default)]
    keep: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawShortcircuit {
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    replacement: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawAnsi {
    #[serde(default)]
    strip: Option<bool>,
}

/// Parse one filter from TOML text. Returns a load-time error string suitable
/// for surfacing in warnings; never panics.
pub fn parse_filter(name: &str, content: &str, source: FilterSource) -> Result<TomlFilter, String> {
    let raw: RawFilter = toml::from_str(content).map_err(|e| format!("invalid TOML: {e}"))?;

    let mut matches = raw.filter.matches;
    if matches.is_empty() {
        // Default to filename-as-program when [filter].matches is omitted.
        matches.push(name.to_string());
    }
    for keyword in &matches {
        if keyword.is_empty() || keyword.contains(char::is_whitespace) {
            return Err(format!("invalid match keyword {keyword:?}"));
        }
    }

    let strip_patterns = raw.strip.unwrap_or_default().patterns;
    if strip_patterns.len() > MAX_PATTERNS_PER_FILTER {
        return Err(format!(
            "too many strip patterns ({} > {MAX_PATTERNS_PER_FILTER})",
            strip_patterns.len()
        ));
    }
    let mut strip = Vec::with_capacity(strip_patterns.len());
    for pattern in strip_patterns {
        let regex =
            build_regex(&pattern, true).map_err(|e| format!("strip pattern {pattern:?}: {e}"))?;
        strip.push(regex);
    }

    let line_max = raw
        .truncate
        .as_ref()
        .and_then(|t| t.line_max)
        .unwrap_or(DEFAULT_LINE_MAX);

    let cap = raw.cap.unwrap_or_default();
    let max_lines = cap.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let keep = match cap.keep.as_deref() {
        None => KeepMode::default(),
        Some("head") => KeepMode::Head,
        Some("tail") => KeepMode::Tail,
        Some("middle") => KeepMode::Middle,
        Some(other) => return Err(format!("invalid cap.keep {other:?}")),
    };

    let shortcircuit = raw.shortcircuit.unwrap_or_default();
    let (shortcircuit_when, shortcircuit_replacement) =
        match (shortcircuit.when, shortcircuit.replacement) {
            (Some(when), Some(replacement)) => {
                let regex = build_regex(&when, false)
                    .map_err(|e| format!("shortcircuit.when {when:?}: {e}"))?;
                (Some(regex), Some(replacement))
            }
            (Some(_), None) => return Err("shortcircuit.when set but replacement missing".into()),
            (None, Some(_)) => return Err("shortcircuit.replacement set but when missing".into()),
            (None, None) => (None, None),
        };

    let strip_ansi = raw.ansi.and_then(|a| a.strip).unwrap_or(true);

    Ok(TomlFilter {
        name: name.to_string(),
        source,
        matches,
        description: raw.filter.description,
        strip,
        line_max,
        max_lines,
        keep,
        shortcircuit_when,
        shortcircuit_replacement,
        strip_ansi,
    })
}

fn build_regex(pattern: &str, multiline: bool) -> Result<Regex, String> {
    RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .multi_line(multiline)
        .build()
        .map_err(|e| e.to_string())
}

/// Run the filter pipeline on `output`. Returns compressed text.
///
/// Pipeline (in order):
/// 1. ANSI strip (if `filter.strip_ansi`)
/// 2. `[strip]` — drop matching lines
/// 3. `[shortcircuit]` — if remainder matches `when`, return `replacement`
/// 4. `[truncate]` — middle-truncate per line at `line_max`
/// 5. `[cap]` — apply `max_lines` with `keep` mode
pub fn apply_filter(filter: &TomlFilter, output: &str) -> String {
    let stripped_ansi = if filter.strip_ansi {
        crate::compress::generic::strip_ansi(output)
    } else {
        output.to_string()
    };

    // Phase 1: line strip
    let kept: Vec<&str> = stripped_ansi
        .lines()
        .filter(|line| !filter.strip.iter().any(|re| re.is_match(line)))
        .collect();
    let after_strip = kept.join("\n");

    // Phase 2: shortcircuit (against the after-strip body)
    if let (Some(when), Some(replacement)) =
        (&filter.shortcircuit_when, &filter.shortcircuit_replacement)
    {
        if when.is_match(&after_strip) {
            return replacement.clone();
        }
    }

    // Phase 3: per-line truncation
    let truncated: Vec<String> = if filter.line_max == usize::MAX {
        kept.iter().map(|s| (*s).to_string()).collect()
    } else {
        kept.iter()
            .map(|line| truncate_line(line, filter.line_max))
            .collect()
    };

    // Phase 4: line cap
    cap_lines(&truncated, filter.max_lines, filter.keep)
}

fn truncate_line(line: &str, line_max: usize) -> String {
    if line.chars().count() <= line_max {
        return line.to_string();
    }
    // Reserve 3 chars for the ellipsis marker.
    let keep_each_side = line_max.saturating_sub(3) / 2;
    let head: String = line.chars().take(keep_each_side).collect();
    let tail: String = line
        .chars()
        .rev()
        .take(keep_each_side)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

fn cap_lines(lines: &[String], max_lines: usize, keep: KeepMode) -> String {
    if lines.len() <= max_lines || max_lines == usize::MAX {
        return lines.join("\n");
    }

    let omitted = lines.len() - max_lines;
    let marker = format!("… ({omitted} more lines)");

    match keep {
        KeepMode::Head => {
            let mut out: Vec<String> = lines.iter().take(max_lines).cloned().collect();
            out.push(marker);
            out.join("\n")
        }
        KeepMode::Tail => {
            let mut out = vec![marker];
            out.extend(lines.iter().skip(omitted).cloned());
            out.join("\n")
        }
        KeepMode::Middle => {
            let head_count = max_lines / 2;
            let tail_count = max_lines - head_count;
            let mut out: Vec<String> = lines.iter().take(head_count).cloned().collect();
            out.push(marker);
            out.extend(lines.iter().skip(lines.len() - tail_count).cloned());
            out.join("\n")
        }
    }
}

/// Extract the program name from a command line, stripping leading env-var
/// assignments (`FOO=bar `) and absolute or relative paths (`/usr/bin/make`,
/// `./node_modules/.bin/eslint`).
///
/// Examples:
/// - `"make build"` → `Some("make")`
/// - `"FOO=1 BAR=2 make"` → `Some("make")`
/// - `"/usr/bin/cargo build"` → `Some("cargo")`
/// - `""` → `None`
pub fn program_name(command: &str) -> Option<&str> {
    for token in command.split_whitespace() {
        // Skip leading env-var assignments (key=value with no shell metachars).
        if is_env_assignment(token) {
            continue;
        }
        // Strip path prefix.
        return Some(basename(token));
    }
    None
}

fn is_env_assignment(token: &str) -> bool {
    let Some(eq) = token.find('=') else {
        return false;
    };
    let key = &token[..eq];
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn basename(token: &str) -> &str {
    // Handle both Unix and Windows separators.
    let last_unix = token.rfind('/');
    let last_win = token.rfind('\\');
    let split_at = match (last_unix, last_win) {
        (Some(u), Some(w)) => u.max(w),
        (Some(u), None) => u,
        (None, Some(w)) => w,
        (None, None) => return token,
    };
    &token[split_at + 1..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> TomlFilter {
        parse_filter("test", content, FilterSource::Builtin).expect("parse")
    }

    #[test]
    fn parses_minimal_filter() {
        let filter = parse(
            r#"
[filter]
matches = ["make"]
"#,
        );
        assert_eq!(filter.matches, vec!["make"]);
        assert_eq!(filter.line_max, usize::MAX);
        assert_eq!(filter.max_lines, usize::MAX);
        assert!(filter.strip.is_empty());
        assert!(filter.shortcircuit_when.is_none());
        assert!(filter.strip_ansi);
    }

    #[test]
    fn filename_default_match() {
        // Empty matches array → filter name is used as the program keyword.
        let filter = parse_filter("ls", "", FilterSource::Builtin).expect("parse");
        assert_eq!(filter.matches, vec!["ls"]);
    }

    #[test]
    fn rejects_invalid_match_keyword() {
        let err = parse_filter(
            "bad",
            r#"[filter]
matches = ["has whitespace"]
"#,
            FilterSource::Builtin,
        )
        .unwrap_err();
        assert!(err.contains("invalid match keyword"), "got: {err}");
    }

    #[test]
    fn rejects_bad_strip_regex() {
        let err = parse_filter(
            "bad",
            r#"
[filter]
matches = ["bad"]

[strip]
patterns = ["[unclosed"]
"#,
            FilterSource::Builtin,
        )
        .unwrap_err();
        assert!(err.contains("strip pattern"), "got: {err}");
    }

    #[test]
    fn strip_drops_matching_lines() {
        let filter = parse(
            r#"
[filter]
matches = ["x"]

[strip]
patterns = ['^Entering directory', '^Leaving directory']
"#,
        );
        let input = "Entering directory `/tmp`\ngcc -c foo.c\nLeaving directory `/tmp`";
        let out = apply_filter(&filter, input);
        assert_eq!(out, "gcc -c foo.c");
    }

    #[test]
    fn shortcircuit_replaces_empty_after_strip() {
        let filter = parse(
            r#"
[filter]
matches = ["x"]

[strip]
patterns = ['^make\[\d+\]:.*']

[shortcircuit]
when = '\A\z'
replacement = "make: ok"
"#,
        );
        let input = "make[1]: Entering directory `/tmp`\nmake[1]: Leaving directory `/tmp`";
        let out = apply_filter(&filter, input);
        assert_eq!(out, "make: ok");
    }

    #[test]
    fn shortcircuit_line_anchors_do_not_match_inner_blank_lines() {
        let filter = parse(
            r#"
[filter]
matches = ["x"]

[shortcircuit]
when = '^\s*$'
replacement = "ok"
"#,
        );
        let out = apply_filter(&filter, "error\n\nhint");
        assert_eq!(out, "error\n\nhint");
    }

    #[test]
    fn cap_tail_keeps_last_n_lines() {
        let filter = parse(
            r#"
[filter]
matches = ["x"]

[cap]
max_lines = 3
keep = "tail"
"#,
        );
        let input = "1\n2\n3\n4\n5";
        let out = apply_filter(&filter, input);
        assert_eq!(out, "… (2 more lines)\n3\n4\n5");
    }

    #[test]
    fn cap_head_keeps_first_n_lines() {
        let filter = parse(
            r#"
[filter]
matches = ["x"]

[cap]
max_lines = 2
keep = "head"
"#,
        );
        let input = "1\n2\n3\n4";
        let out = apply_filter(&filter, input);
        assert_eq!(out, "1\n2\n… (2 more lines)");
    }

    #[test]
    fn cap_middle_keeps_head_and_tail() {
        let filter = parse(
            r#"
[filter]
matches = ["x"]

[cap]
max_lines = 4
keep = "middle"
"#,
        );
        let input = "1\n2\n3\n4\n5\n6\n7\n8";
        let out = apply_filter(&filter, input);
        // 4/2=2 head, 4-2=2 tail → keep 1,2 then 7,8
        assert_eq!(out, "1\n2\n… (4 more lines)\n7\n8");
    }

    #[test]
    fn truncate_per_line() {
        let filter = parse(
            r#"
[filter]
matches = ["x"]

[truncate]
line_max = 10
"#,
        );
        let input = "shortline\nthis is a very long line indeed";
        let out = apply_filter(&filter, input);
        assert!(out.contains("shortline"));
        assert!(out.contains("…"));
        assert!(out.lines().any(|l| l.chars().count() <= 10));
    }

    #[test]
    fn ansi_strip_default_true() {
        let filter = parse(
            r#"
[filter]
matches = ["x"]
"#,
        );
        let input = "\x1b[31mred\x1b[0m text";
        let out = apply_filter(&filter, input);
        assert_eq!(out, "red text");
    }

    #[test]
    fn ansi_strip_can_be_disabled() {
        let filter = parse(
            r#"
[filter]
matches = ["x"]

[ansi]
strip = false
"#,
        );
        let input = "\x1b[31mred\x1b[0m text";
        let out = apply_filter(&filter, input);
        assert_eq!(out, input);
    }

    #[test]
    fn shortcircuit_runs_on_after_strip_body() {
        // After stripping all lines we have empty string; shortcircuit `^$` matches.
        let filter = parse(
            r#"
[filter]
matches = ["x"]

[strip]
patterns = ['^.*$']

[shortcircuit]
when = '^$'
replacement = "ok"
"#,
        );
        assert_eq!(apply_filter(&filter, "anything\nat all"), "ok");
    }

    #[test]
    fn program_name_handles_env_and_paths() {
        assert_eq!(program_name("make build"), Some("make"));
        assert_eq!(program_name("FOO=1 BAR=2 make build"), Some("make"));
        assert_eq!(program_name("/usr/bin/cargo build"), Some("cargo"));
        assert_eq!(program_name("./node_modules/.bin/eslint ."), Some("eslint"));
        // Path is the program; subsequent tokens are arguments.
        assert_eq!(program_name("FOO=bar /opt/x/y subcmd"), Some("y"));
        assert_eq!(program_name(""), None);
        assert_eq!(program_name("   "), None);
    }

    #[test]
    fn program_name_unquoted_windows_path() {
        // Unquoted Windows paths with spaces won't round-trip cleanly because
        // split_whitespace breaks on the embedded space. This is acceptable —
        // bash would fail to execute these without quoting too, and AFT's
        // shell handlers run the literal command. Document the behavior.
        // basename strips through the last backslash even on the broken-by-whitespace
        // first token, leaving "Program".
        assert_eq!(
            program_name(r"C:\Program Files\Git\bin\git.exe status"),
            Some("Program")
        );
    }

    #[test]
    fn program_name_does_not_skip_non_assignment_token_with_equals() {
        // `=value` (no key) is not an env assignment.
        assert_eq!(program_name("=oops echo hi"), Some("=oops"));
    }

    #[test]
    fn registry_lookup_by_program_name() {
        let registry = build_registry(
            &[(
                "make",
                r#"[filter]
matches = ["make"]

[strip]
patterns = ['^Entering']
"#,
            )],
            None,
            None,
        );
        let f = registry.lookup("make build foo").unwrap();
        assert_eq!(f.matches, vec!["make"]);
        assert!(matches!(f.source, FilterSource::Builtin));
    }

    #[test]
    fn registry_user_overrides_builtin() {
        let tmp = tempfile::tempdir().unwrap();
        let user_path = tmp.path().join("make.toml");
        fs::write(
            &user_path,
            r#"[filter]
matches = ["make"]
description = "user override"
"#,
        )
        .unwrap();

        let registry = build_registry(
            &[(
                "make",
                r#"[filter]
matches = ["make"]
description = "builtin"
"#,
            )],
            Some(tmp.path()),
            None,
        );
        let f = registry.lookup("make build").unwrap();
        assert_eq!(f.description.as_deref(), Some("user override"));
        assert!(matches!(f.source, FilterSource::User { .. }));
    }

    #[test]
    fn registry_project_overrides_user() {
        let user_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        fs::write(
            user_dir.path().join("make.toml"),
            r#"[filter]
matches = ["make"]
description = "user"
"#,
        )
        .unwrap();
        fs::write(
            project_dir.path().join("make.toml"),
            r#"[filter]
matches = ["make"]
description = "project"
"#,
        )
        .unwrap();

        let registry = build_registry(&[], Some(user_dir.path()), Some(project_dir.path()));
        let f = registry.lookup("make").unwrap();
        assert_eq!(f.description.as_deref(), Some("project"));
        assert!(matches!(f.source, FilterSource::Project { .. }));
    }

    #[test]
    fn bad_filter_files_warn_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("good.toml"),
            r#"[filter]
matches = ["good"]
"#,
        )
        .unwrap();
        fs::write(tmp.path().join("bad.toml"), "not valid = toml = at all =").unwrap();

        let registry = build_registry(&[], Some(tmp.path()), None);
        assert!(registry.lookup("good").is_some());
        assert!(registry.lookup("bad").is_none());
        assert!(
            registry.warnings().iter().any(|w| w.contains("bad.toml")),
            "warnings: {:?}",
            registry.warnings()
        );
    }

    #[test]
    fn missing_dir_does_not_warn() {
        let registry = build_registry(&[], Some(Path::new("/nonexistent/path/12345")), None);
        assert!(registry.warnings().is_empty());
    }
}
