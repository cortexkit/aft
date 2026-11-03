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
//! - **project**: `<project>/.cortexkit/aft/filters/*.toml` — trust-gated, see [`crate::compress::trust`]
//!
//! Bad filters are skipped with a warning, never panic.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use regex::{Regex, RegexBuilder};
use serde::Deserialize;

use crate::compress::caps::{cap_classified_blocks_with, ClassifiedBlock, DropClass};
use crate::compress::CompressionResult;

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
    pub strip: Vec<Arc<Regex>>,
    pub line_max: usize,
    pub max_lines: usize,
    pub keep: KeepMode,
    pub class_cap: Option<TomlClassCap>,
    pub shortcircuit_when: Option<Arc<Regex>>,
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

#[derive(Debug, Clone)]
pub struct TomlClassCap {
    pub class: DropClass,
    pub max: usize,
    pub patterns: Vec<Arc<Regex>>,
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

    /// Private registry buffers, excluding shared compiled regex engines.
    pub(crate) fn estimated_memory(&self) -> crate::memory::MemoryEstimate {
        let filter_bytes = |filter: &TomlFilter| {
            let source = match &filter.source {
                FilterSource::Builtin => 0,
                FilterSource::User { path } | FilterSource::Project { path } => path.capacity(),
            };
            source
                .saturating_add(filter.name.capacity())
                .saturating_add(filter.description.as_ref().map_or(0, String::capacity))
                .saturating_add(
                    filter
                        .matches
                        .capacity()
                        .saturating_mul(std::mem::size_of::<String>()),
                )
                .saturating_add(filter.matches.iter().map(String::capacity).sum::<usize>())
                .saturating_add(
                    filter
                        .strip
                        .capacity()
                        .saturating_mul(std::mem::size_of::<Arc<Regex>>()),
                )
                .saturating_add(filter.class_cap.as_ref().map_or(0, |cap| {
                    cap.patterns
                        .capacity()
                        .saturating_mul(std::mem::size_of::<Arc<Regex>>())
                }))
                .saturating_add(
                    filter
                        .shortcircuit_replacement
                        .as_ref()
                        .map_or(0, String::capacity),
                )
        };
        let buckets = if self.by_match.capacity() == 0 {
            0
        } else {
            self.by_match
                .capacity()
                .saturating_add(1)
                .checked_next_power_of_two()
                .unwrap_or(usize::MAX)
        };
        let bytes = buckets
            .saturating_mul(std::mem::size_of::<(String, TomlFilter)>().saturating_add(1))
            .saturating_add(
                self.by_match
                    .iter()
                    .map(|(key, filter)| key.capacity().saturating_add(filter_bytes(filter)))
                    .sum::<usize>(),
            )
            .saturating_add(
                self.all
                    .capacity()
                    .saturating_mul(std::mem::size_of::<TomlFilter>()),
            )
            .saturating_add(self.all.iter().map(filter_bytes).sum::<usize>())
            .saturating_add(
                self.warnings
                    .capacity()
                    .saturating_mul(std::mem::size_of::<String>()),
            )
            .saturating_add(self.warnings.iter().map(String::capacity).sum::<usize>());
        crate::memory::MemoryEstimate::partial(bytes as u64)
            .count("filters", self.all.len())
            .count("command_keywords", self.by_match.len())
            .gap("compiled_regex_automata")
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

    load_overrides(registry, user_dir, project_dir)
}

fn load_overrides(
    mut registry: FilterRegistry,
    user_dir: Option<&Path>,
    project_dir: Option<&Path>,
) -> FilterRegistry {
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

/// Build the shipped filters with fresh user and project overrides.
pub fn build_standard_registry(
    user_dir: Option<&Path>,
    project_dir: Option<&Path>,
) -> FilterRegistry {
    // Only shipped, immutable definitions are shared. User/project files are
    // re-read on each reset so trust changes and edits remain root-specific.
    static BUILTINS: OnceLock<FilterRegistry> = OnceLock::new();
    let builtins = BUILTINS.get_or_init(|| build_registry(super::builtin_filters::ALL, None, None));
    load_overrides(builtins.clone(), user_dir, project_dir)
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
    class_cap: Option<RawClassCap>,
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
struct RawClassCap {
    #[serde(default)]
    class: Option<String>,
    #[serde(default)]
    max: Option<usize>,
    #[serde(default)]
    patterns: Vec<String>,
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

    let class_cap = match raw.class_cap {
        Some(raw_class_cap) => {
            if raw_class_cap.patterns.len() > MAX_PATTERNS_PER_FILTER {
                return Err(format!(
                    "too many class_cap patterns ({} > {MAX_PATTERNS_PER_FILTER})",
                    raw_class_cap.patterns.len()
                ));
            }
            let class = parse_drop_class(raw_class_cap.class.as_deref().unwrap_or("list"))?;
            let mut patterns = Vec::with_capacity(raw_class_cap.patterns.len());
            for pattern in raw_class_cap.patterns {
                let regex = build_regex(&pattern, true)
                    .map_err(|e| format!("class_cap pattern {pattern:?}: {e}"))?;
                patterns.push(regex);
            }
            Some(TomlClassCap {
                class,
                max: raw_class_cap.max.unwrap_or_else(|| class.default_cap()),
                patterns,
            })
        }
        None => None,
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
        class_cap,
        shortcircuit_when,
        shortcircuit_replacement,
        strip_ansi,
    })
}

fn build_regex(pattern: &str, multiline: bool) -> Result<Arc<Regex>, String> {
    RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .multi_line(multiline)
        .build()
        .map(Arc::new)
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
pub fn apply_filter(filter: &TomlFilter, output: &str) -> CompressionResult {
    apply_filter_with_exit_code(filter, output, None)
}

pub fn apply_filter_with_exit_code(
    filter: &TomlFilter,
    output: &str,
    exit_code: Option<i64>,
) -> CompressionResult {
    if filter.strip_ansi {
        let stripped = crate::compress::generic::strip_ansi(output);
        apply_filter_to_text(filter, &stripped, exit_code)
    } else {
        apply_filter_to_text(filter, output, exit_code)
    }
}

pub(crate) fn apply_filter_with_exit_code_prestripped(
    filter: &TomlFilter,
    raw_output: &str,
    stripped_output: &str,
    exit_code: Option<i64>,
) -> CompressionResult {
    let output = if filter.strip_ansi {
        stripped_output
    } else {
        raw_output
    };
    apply_filter_to_text(filter, output, exit_code)
}

fn apply_filter_to_text(
    filter: &TomlFilter,
    output: &str,
    exit_code: Option<i64>,
) -> CompressionResult {
    if filter.shortcircuit_when.is_none()
        && filter.class_cap.is_none()
        && filter.max_lines != usize::MAX
    {
        return apply_plain_cap_streaming(filter, output);
    }

    // Phase 1: line strip
    let original_line_count = output.lines().count();
    let kept: Vec<&str> = output
        .lines()
        .filter(|line| !filter.strip.iter().any(|re| re.is_match(line)))
        .collect();
    let strip_removed_lines = kept.len() < original_line_count;
    // Phase 2: shortcircuit (against the after-strip body)
    if let (Some(when), Some(replacement)) =
        (&filter.shortcircuit_when, &filter.shortcircuit_replacement)
    {
        let after_strip = kept.join("\n");
        let shortcircuit_safe = match exit_code {
            Some(code) => code == 0,
            None => !super::text_has_failure_signal(&after_strip),
        };
        if shortcircuit_safe && when.is_match(&after_strip) {
            return CompressionResult::new(replacement.clone());
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

    // Phase 4: class cap replaces plain [cap] when present; the two never stack.
    if let Some(class_cap) = &filter.class_cap {
        return cap_class_lines(&truncated, class_cap);
    }

    // Phase 5: plain line cap
    cap_lines(
        &truncated,
        filter.max_lines,
        filter.keep,
        strip_removed_lines,
    )
}

/// Apply a finite plain cap without materializing lines that the cap will discard.
fn apply_plain_cap_streaming(filter: &TomlFilter, output: &str) -> CompressionResult {
    let max_lines = filter.max_lines;
    let head_count = match filter.keep {
        KeepMode::Head => max_lines,
        KeepMode::Tail => 0,
        KeepMode::Middle => max_lines / 2,
    };
    // Head mode buffers the head plus a tail window's worth of the latest lines;
    // the shared head/tail plan then decides the real split below.
    let tail_count = match filter.keep {
        KeepMode::Head => crate::compress::line_cut::TAIL_WINDOW_LINES.min(max_lines),
        KeepMode::Tail => max_lines,
        KeepMode::Middle => max_lines - head_count,
    };
    let mut head = Vec::new();
    let mut tail = VecDeque::new();
    let mut kept_line_count = 0usize;
    let mut strip_removed_lines = false;

    for line in output.lines() {
        if filter.strip.iter().any(|pattern| pattern.is_match(line)) {
            strip_removed_lines = true;
            continue;
        }

        kept_line_count += 1;
        if head.len() < head_count {
            head.push(line);
            continue;
        }
        if tail_count == 0 {
            continue;
        }
        if tail.len() == tail_count {
            tail.pop_front();
        }
        tail.push_back(line);
    }

    head.extend(tail);
    let selected: Vec<String> = head
        .into_iter()
        .map(|line| truncate_line(line, filter.line_max))
        .collect();

    if kept_line_count <= max_lines {
        return CompressionResult::new(selected.join("\n"));
    }
    if max_lines == 0 {
        return CompressionResult::with_inner_drop(String::new(), false);
    }
    if matches!(filter.keep, KeepMode::Head) {
        // `selected` holds the first `max_lines` lines followed by up to a tail
        // window of the last lines, so planning over it picks the same lines as
        // planning over the whole output; only the omitted count must come from
        // the real line total.
        let plan = crate::compress::line_cut::plan_head_tail(selected.len(), max_lines, |index| {
            selected[index].len()
        })
        .expect("more lines than the cap");
        let omitted = kept_line_count - plan.head - plan.tail;
        let kept = crate::compress::line_cut::render_cut(&selected, plan, omitted, "lines");
        return CompressionResult::with_inner_drop(kept.join("\n"), false);
    }
    let text = selected.join("\n");
    if matches!(filter.keep, KeepMode::Tail) && !strip_removed_lines {
        return CompressionResult::with_prefix_drop(
            text,
            kept_line_count.saturating_sub(max_lines) + 1,
        );
    }
    CompressionResult::with_inner_drop(text, false)
}

fn truncate_line(line: &str, line_max: usize) -> String {
    record_truncate_line_call();
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

#[cfg(test)]
thread_local! {
    static TRUNCATE_LINE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_truncate_line_call() {
    TRUNCATE_LINE_CALLS.with(|calls| calls.set(calls.get() + 1));
}

#[cfg(not(test))]
fn record_truncate_line_call() {}

#[cfg(test)]
fn reset_truncate_line_calls() {
    TRUNCATE_LINE_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
fn truncate_line_calls() -> usize {
    TRUNCATE_LINE_CALLS.with(std::cell::Cell::get)
}

fn cap_class_lines(lines: &[String], class_cap: &TomlClassCap) -> CompressionResult {
    let blocks = lines
        .iter()
        .map(|line| {
            if class_cap.patterns.is_empty()
                || class_cap
                    .patterns
                    .iter()
                    .any(|pattern| pattern.is_match(line))
            {
                ClassifiedBlock::new(class_cap.class, line.clone())
            } else {
                ClassifiedBlock::unclassified(line.clone())
            }
        })
        .collect();
    let capped = cap_classified_blocks_with(blocks, |class| {
        if class == class_cap.class {
            class_cap.max
        } else {
            class.default_cap()
        }
    });
    CompressionResult::with_class_drops(capped.text, capped.dropped_by_class)
}

fn cap_lines(
    lines: &[String],
    max_lines: usize,
    keep: KeepMode,
    had_prior_line_drop: bool,
) -> CompressionResult {
    if lines.len() <= max_lines || max_lines == usize::MAX {
        return CompressionResult::new(lines.join("\n"));
    }

    if max_lines == 0 {
        return CompressionResult::with_inner_drop(String::new(), false);
    }

    let kept = match keep {
        // A head cap still keeps the final lines: tools print their verdict
        // last, and a filter that asked for the head would otherwise hide it.
        KeepMode::Head => crate::compress::line_cut::cap_lines_head_tail(lines, max_lines, "lines"),
        KeepMode::Tail => lines
            .iter()
            .skip(lines.len().saturating_sub(max_lines))
            .cloned()
            .collect::<Vec<_>>(),
        KeepMode::Middle => {
            let head_count = max_lines / 2;
            let tail_count = max_lines - head_count;
            let mut kept: Vec<String> = lines.iter().take(head_count).cloned().collect();
            kept.extend(lines.iter().skip(lines.len() - tail_count).cloned());
            kept
        }
    };
    if matches!(keep, KeepMode::Tail) && !had_prior_line_drop {
        let dropped_prefix_lines = lines.len().saturating_sub(max_lines);
        CompressionResult::with_prefix_drop(kept.join("\n"), dropped_prefix_lines + 1)
    } else {
        CompressionResult::with_inner_drop(kept.join("\n"), false)
    }
}

fn parse_drop_class(value: &str) -> Result<DropClass, String> {
    match value {
        "error" | "errors" => Ok(DropClass::Error),
        "warning" | "warnings" => Ok(DropClass::Warning),
        "failure" | "failures" => Ok(DropClass::Failure),
        "issue" | "issues" => Ok(DropClass::Issue),
        "list" | "list_item" | "list-items" | "list items" => Ok(DropClass::List),
        "inventory" | "inventory_item" | "inventory-items" | "inventory items" => {
            Ok(DropClass::Inventory)
        }
        "timing" | "timing_line" | "timing-lines" | "timing lines" => Ok(DropClass::Timing),
        other => Err(format!("invalid class_cap.class {other:?}")),
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

    fn materialized_plain_cap_reference(filter: &TomlFilter, output: &str) -> CompressionResult {
        let original_line_count = output.lines().count();
        let kept: Vec<&str> = output
            .lines()
            .filter(|line| !filter.strip.iter().any(|pattern| pattern.is_match(line)))
            .collect();
        let strip_removed_lines = kept.len() < original_line_count;
        let truncated: Vec<String> = kept
            .iter()
            .map(|line| truncate_line(line, filter.line_max))
            .collect();
        cap_lines(
            &truncated,
            filter.max_lines,
            filter.keep,
            strip_removed_lines,
        )
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
        let out = apply_filter(&filter, input).text;
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
        let out = apply_filter(&filter, input).text;
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
        let out = apply_filter(&filter, "error\n\nhint").text;
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
        assert_eq!(out.text, "3\n4\n5");
        assert!(out.had_inner_drop);
        assert!(out.offset_hint_eligible);
        assert_eq!(out.text.lines().count(), 3);
    }

    #[test]
    fn cap_tail_after_strip_disables_offset_hint() {
        let filter = parse(
            r#"
[filter]
matches = ["x"]

[strip]
patterns = ["^strip-me"]

[cap]
max_lines = 2
keep = "tail"
"#,
        );
        let out = apply_filter(
            &filter,
            "strip-me
1
2
3
4",
        );

        assert_eq!(
            out.text,
            "3
4"
        );
        assert!(out.had_inner_drop);
        assert!(!out.offset_hint_eligible);
        assert_eq!(out.offset_start_line, None);
    }

    #[test]
    fn cap_head_keeps_head_and_final_line() {
        // keep = "head" used to drop the last lines outright. Tools print their
        // verdict last, so a head cap now keeps a tail window too and marks
        // the cut in the middle; the marker is not one of the shown lines.
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
        assert_eq!(out.text, "1\n... 2 lines omitted ...\n4");
        assert!(out.had_inner_drop);
        assert!(!out.offset_hint_eligible);
        assert_eq!(crate::list_surfaces::bash::count_output_lines(&out.text), 2);
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
        assert_eq!(out.text, "1\n2\n7\n8");
        assert!(out.had_inner_drop);
        assert!(!out.offset_hint_eligible);
        assert_eq!(out.text.lines().count(), 4);
    }

    #[test]
    fn cap_zero_keeps_no_lines() {
        let filter = parse(
            r#"
[filter]
matches = ["x"]

[cap]
max_lines = 0
keep = "head"
"#,
        );
        let out = apply_filter(&filter, "1\n2\n3");
        assert_eq!(out.text, "");
        assert!(out.had_inner_drop);
    }

    #[test]
    fn cap_one_keeps_one_tail_line_without_marker() {
        let filter = parse(
            r#"
[filter]
matches = ["x"]

[cap]
max_lines = 1
keep = "tail"
"#,
        );
        let out = apply_filter(&filter, "1\n2\n3");
        assert_eq!(out.text, "3");
        assert!(out.had_inner_drop);
        assert!(out.offset_hint_eligible);
        assert_eq!(out.text.lines().count(), 1);
    }

    #[test]
    fn cap_two_keeps_two_tail_lines_without_marker() {
        let filter = parse(
            r#"
[filter]
matches = ["x"]

[cap]
max_lines = 2
keep = "tail"
"#,
        );
        let out = apply_filter(&filter, "1\n2\n3\n4");
        assert_eq!(out.text, "3\n4");
        assert!(out.had_inner_drop);
        assert!(out.offset_hint_eligible);
        assert_eq!(out.text.lines().count(), 2);
    }

    #[test]
    fn class_cap_replaces_plain_cap_without_stacking() {
        let filter = parse(
            r#"
[filter]
matches = ["x"]

[class_cap]
class = "warning"
max = 2
patterns = ["^warning"]

[cap]
max_lines = 1
keep = "head"
"#,
        );
        let out = apply_filter(&filter, "warning 1\nkeep me\nwarning 2\nwarning 3");

        assert!(out.text.contains("warning 1"));
        assert!(out.text.contains("keep me"));
        assert!(out.text.contains("warning 2"));
        assert!(!out.text.contains("warning 3"));
        assert_eq!(out.dropped_by_class.get(&DropClass::Warning), Some(&1));
        assert!(out.text.lines().count() > 1, "plain [cap] must not stack");
    }

    #[test]
    fn streaming_plain_caps_match_materialized_reference() {
        let input = "keep first\ndrop chatter\nkeep a very long line\nkeep middle\n\
                     drop more chatter\nkeep penultimate\nkeep last\n";
        for keep in ["head", "tail", "middle"] {
            for max_lines in 0..=6 {
                let filter = parse(&format!(
                    r#"
[filter]
matches = ["x"]

[strip]
patterns = ["^drop"]

[truncate]
line_max = 10

[cap]
max_lines = {max_lines}
keep = "{keep}"
"#
                ));

                assert_eq!(
                    apply_filter_to_text(&filter, input, None),
                    materialized_plain_cap_reference(&filter, input),
                    "keep={keep}, max_lines={max_lines}"
                );
            }
        }
    }

    #[test]
    fn finite_plain_cap_only_truncates_retained_lines() {
        let filter = parse(
            r#"
[filter]
matches = ["x"]

[cap]
max_lines = 3
keep = "tail"
"#,
        );
        let input = (0..100)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        reset_truncate_line_calls();

        let output = apply_filter(&filter, &input);

        assert_eq!(output.text, "line 97\nline 98\nline 99");
        assert_eq!(truncate_line_calls(), 3);
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
        let out = apply_filter(&filter, input).text;
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
        let out = apply_filter(&filter, input).text;
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
        let out = apply_filter(&filter, input).text;
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
        assert_eq!(apply_filter(&filter, "anything\nat all").text, "ok");
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
    fn standard_registry_reuses_compiled_builtins_but_reloads_local_overrides() {
        let first = build_standard_registry(None, None);
        let (second, allocated) =
            crate::test_allocations::count_bytes(|| build_standard_registry(None, None));
        assert_eq!(first.all().len(), second.all().len());
        assert!(
            allocated < 512 * 1024,
            "warm builtin registry allocated {allocated} bytes; compiled regexes must be shared"
        );
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("make.toml");
        for description in ["first override", "changed override"] {
            fs::write(
                &file,
                format!("[filter]\nmatches = [\"make\"]\ndescription = \"{description}\"\n"),
            )
            .unwrap();
            let overridden = build_standard_registry(Some(dir.path()), None);
            assert_eq!(
                overridden.lookup("make").unwrap().description.as_deref(),
                Some(description)
            );
            assert!(matches!(
                overridden.lookup("make").unwrap().source,
                FilterSource::User { .. }
            ));
            assert!(matches!(
                first.lookup("make").unwrap().source,
                FilterSource::Builtin
            ));
        }
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
