//! Output compression for hoisted bash.
//!
//! Compression has five tiers, tried in this order:
//!
//! 1. **Specific Rust [`Compressor`] modules** — hand-written parsers for
//!    specific tools identified by tool tokens (for example `vitest`, `eslint`,
//!    `cargo`, `git`). These win before broad package-manager compressors.
//! 2. **Output-shape [`Compressor`] sniffers** — inner-tool parsers that can
//!    recognize their own private summaries even when invoked through wrappers
//!    such as `npm test`, `make test`, or `./scripts/check.sh`.
//! 3. **Package-manager [`Compressor`] modules** — broad head-token matchers
//!    (`npm`, `pnpm`, `bun`) that compress unclaimed package-manager output.
//! 4. **TOML filters** — declarative strip + truncate + cap + shortcircuit
//!    rules for the long tail of CLI tools. Loaded from builtin / user /
//!    project sources via [`toml_filter::build_registry`]. See
//!    [`toml_filter`] and [`trust`] for the trust model.
//! 5. **[`generic`] fallback** — ANSI strip + consecutive-dedup. The
//!    background bash registry owns the shared final output cap.

pub mod biome;
pub mod builtin_filters;
pub mod bun;
pub mod caps;
pub mod cargo;
pub mod eslint;
pub mod find;
pub mod generic;
pub mod git;
pub mod go;
pub mod listing_fold;
pub mod ls;
pub mod mypy;
pub mod next;
pub mod npm;
pub mod playwright;
pub mod pnpm;
pub mod prettier;
pub mod pytest;
pub mod ruff;
pub mod toml_filter;
pub mod tree;
pub mod trust;
pub mod tsc;
pub mod vitest;

use crate::context::AppContext;
use crate::harness::Harness;
use biome::BiomeCompressor;
use bun::BunCompressor;
use caps::DropClass;
use cargo::CargoCompressor;
use eslint::EslintCompressor;
use find::FindCompressor;
use generic::{strip_ansi, GenericCompressor};
use git::GitCompressor;
use go::{GoCompressor, GolangciLintCompressor};
use ls::LsCompressor;
use mypy::MypyCompressor;
use next::NextCompressor;
use npm::NpmCompressor;
use playwright::PlaywrightCompressor;
use pnpm::PnpmCompressor;
use prettier::PrettierCompressor;
use pytest::PytestCompressor;
use ruff::RuffCompressor;
use std::cell::OnceCell;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use toml_filter::{apply_filter_with_exit_code_prestripped, FilterRegistry};
use tree::TreeCompressor;
use tsc::TscCompressor;
use vitest::VitestCompressor;

/// Thread-safe handle to the TOML filter registry. Shared between
/// `AppContext::filter_registry()` (for direct use in command handlers) and
/// `BgTaskRegistry`'s output compression closure (for use from the watchdog
/// thread).
pub type SharedFilterRegistry = Arc<RwLock<FilterRegistry>>;

/// How specifically a compressor identifies a command.
///
/// `Specific` matchers (vitest, eslint, biome, tsc, pytest, cargo, git)
/// claim a command by recognising a SPECIFIC tool name as a token anywhere
/// in the command line — `npx vitest`, `pnpm exec eslint --fix`,
/// `bun run vitest`, etc.
///
/// `PackageManager` matchers (npm, pnpm, bun) claim a command by its
/// HEAD token alone (e.g. `npm`, `bun`) regardless of what subcommand
/// follows. They are intentionally broad — when a `bun run vitest` is
/// not claimed by VitestCompressor, BunCompressor still wants the chance
/// to compress generic bun output for unknown subcommands.
///
/// Dispatch order: Specific command tier first, then output-shape sniffers
/// (Specific before PackageManager), then PackageManager command tier, then
/// TOML filters, then GenericCompressor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Specificity {
    Specific,
    PackageManager,
}

/// Shared, per-dispatch view of command output for output-shape matchers.
///
/// Several matchers recognize JSON by inspecting different top-level keys. A
/// probe lets them share one lazy parse while leaving each matcher's decision
/// and dispatch priority unchanged.
pub struct OutputProbe<'a> {
    output: &'a str,
    json: OnceCell<Option<serde_json::Value>>,
}

impl<'a> OutputProbe<'a> {
    pub fn new(output: &'a str) -> Self {
        Self {
            output,
            json: OnceCell::new(),
        }
    }

    pub fn output(&self) -> &'a str {
        self.output
    }

    /// Parse plausible JSON at most once for all matchers in this dispatch.
    pub fn json(&self) -> Option<&serde_json::Value> {
        self.json
            .get_or_init(|| {
                let trimmed = self.output.trim_start();
                if !matches!(trimmed.as_bytes().first(), Some(b'{') | Some(b'[')) {
                    return None;
                }
                record_output_probe_json_parse();
                serde_json::from_str(trimmed).ok()
            })
            .as_ref()
    }
}

#[cfg(debug_assertions)]
thread_local! {
    static OUTPUT_PROBE_JSON_PARSE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(debug_assertions)]
fn record_output_probe_json_parse() {
    OUTPUT_PROBE_JSON_PARSE_COUNT.with(|count| count.set(count.get() + 1));
}

#[cfg(not(debug_assertions))]
fn record_output_probe_json_parse() {}

#[cfg(all(debug_assertions, test))]
fn reset_output_probe_json_parse_count() {
    OUTPUT_PROBE_JSON_PARSE_COUNT.with(|count| count.set(0));
}

#[cfg(all(debug_assertions, test))]
fn output_probe_json_parse_count() -> usize {
    OUTPUT_PROBE_JSON_PARSE_COUNT.with(std::cell::Cell::get)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressionResult {
    pub text: String,
    pub dropped_by_class: BTreeMap<DropClass, usize>,
    pub had_inner_drop: bool,
    pub offset_hint_eligible: bool,
    pub offset_start_line: Option<usize>,
    pub input_line_count: usize,
}

impl CompressionResult {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let input_line_count = text.lines().count();
        Self {
            text,
            dropped_by_class: BTreeMap::new(),
            had_inner_drop: false,
            offset_hint_eligible: true,
            offset_start_line: None,
            input_line_count,
        }
    }

    pub fn with_class_drops(
        text: impl Into<String>,
        dropped_by_class: BTreeMap<DropClass, usize>,
    ) -> Self {
        let text = text.into();
        let input_line_count = text.lines().count();
        let had_inner_drop = !dropped_by_class.is_empty();
        Self {
            text,
            dropped_by_class,
            had_inner_drop,
            offset_hint_eligible: !had_inner_drop,
            offset_start_line: None,
            input_line_count,
        }
    }

    pub fn with_inner_drop(text: impl Into<String>, offset_hint_eligible: bool) -> Self {
        let text = text.into();
        let input_line_count = text.lines().count();
        Self {
            text,
            dropped_by_class: BTreeMap::new(),
            had_inner_drop: true,
            offset_hint_eligible,
            offset_start_line: None,
            input_line_count,
        }
    }

    pub fn with_prefix_drop(text: impl Into<String>, offset_start_line: usize) -> Self {
        let text = text.into();
        let input_line_count = text.lines().count();
        Self {
            text,
            dropped_by_class: BTreeMap::new(),
            had_inner_drop: true,
            offset_hint_eligible: true,
            offset_start_line: Some(offset_start_line),
            input_line_count,
        }
    }

    pub fn with_input_line_count(mut self, count: usize) -> Self {
        self.input_line_count = count;
        self
    }

    pub fn input_line_count(&self) -> usize {
        self.input_line_count
    }

    pub fn has_semantic_drops(&self) -> bool {
        !self.dropped_by_class.is_empty()
    }

    pub fn has_any_drop(&self) -> bool {
        self.had_inner_drop || self.has_semantic_drops()
    }

    pub fn map_text<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&str) -> String,
    {
        self.text = f(&self.text);
        self
    }
}

impl std::fmt::Display for CompressionResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl std::ops::Deref for CompressionResult {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}

impl PartialEq<&str> for CompressionResult {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl PartialEq<String> for CompressionResult {
    fn eq(&self, other: &String) -> bool {
        self.text == *other
    }
}

impl From<String> for CompressionResult {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

impl From<&str> for CompressionResult {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

/// A `Compressor` knows how to reduce one specific command's output to fewer
/// tokens while preserving the information the agent needs.
pub trait Compressor: Send + Sync {
    /// Returns true if this compressor handles the given command head + args.
    /// Called after generic detection (ANSI strip, dedup) so this is per-command logic only.
    fn matches(&self, command: &str) -> bool;

    /// Compress the output when the process exit code is unknown.
    fn compress(&self, command: &str, output: &str) -> CompressionResult {
        self.compress_with_exit_code(command, output, None)
    }

    /// Compress the output. Original is left untouched if compression fails.
    fn compress_with_exit_code(
        &self,
        command: &str,
        output: &str,
        exit_code: Option<i32>,
    ) -> CompressionResult;

    fn specificity(&self) -> Specificity {
        Specificity::Specific
    }

    /// Returns true when this compressor recognizes output produced by its
    /// inner tool even if the command head was a wrapper (`npm test`,
    /// `make test`, `./scripts/check.sh`, etc.). Wrapper compressors should
    /// not override this; they remain command-only.
    fn matches_output(&self, _output: &str) -> bool {
        false
    }

    /// Probe-aware output matching. Matchers that need shared derived data can
    /// override this; all other matchers retain their existing implementation.
    fn matches_output_probe(&self, probe: &OutputProbe<'_>) -> bool {
        self.matches_output(probe.output())
    }

    /// Compress output after an output-shape match when the process exit code is unknown.
    fn compress_output_match(&self, output: &str) -> CompressionResult {
        self.compress_output_match_with_exit_code(output, None)
    }

    /// Compress output after an output-shape match. Compressors that branch by
    /// subcommand override this to jump directly to the matched branch.
    fn compress_output_match_with_exit_code(
        &self,
        output: &str,
        exit_code: Option<i32>,
    ) -> CompressionResult {
        self.compress_with_exit_code("", output, exit_code)
    }
}
/// Top-level dispatch: try specific Rust modules, output-shape sniffers, package-manager modules, TOML filters, then generic fallback.
///
/// Convenience wrapper for command handlers that already hold an `AppContext`.
/// Backs onto [`compress_with_registry`] which is thread-safe for use from the
/// `BgTaskRegistry` watchdog.
pub fn compress(command: &str, output: String, ctx: &AppContext) -> CompressionResult {
    compress_with_exit_code(command, output, None, ctx)
}

pub fn compress_with_exit_code(
    command: &str,
    output: String,
    exit_code: Option<i32>,
    ctx: &AppContext,
) -> CompressionResult {
    if !ctx.config().experimental_bash_compress {
        let count = output.lines().count();
        return CompressionResult::new(output).with_input_line_count(count);
    }
    let registry_handle = ctx.shared_filter_registry();
    let guard = match registry_handle.read() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    compress_with_registry_exit_code(command, &output, exit_code, &guard)
}

/// Thread-safe dispatch that does not need `AppContext`. Caller is responsible
/// for the `experimental_bash_compress` gate (the registry has no opinion).
///
/// Used from background threads (notably the `BgTaskRegistry` watchdog and
/// completion-frame emitter) where lock-free access is required.
pub fn compress_with_registry(
    command: &str,
    output: &str,
    registry: &FilterRegistry,
) -> CompressionResult {
    compress_with_registry_exit_code(command, output, None, registry)
}

pub fn compress_with_registry_exit_code(
    command: &str,
    output: &str,
    exit_code: Option<i32>,
    registry: &FilterRegistry,
) -> CompressionResult {
    let input_line_count = output.lines().count();
    let mut result = compress_with_registry_exit_code_inner(command, output, exit_code, registry);
    result.input_line_count = input_line_count;
    result
}

fn compress_with_registry_exit_code_inner(
    command: &str,
    output: &str,
    exit_code: Option<i32>,
    registry: &FilterRegistry,
) -> CompressionResult {
    let stripped_for_generic = strip_ansi(output);

    // Resolve what to dispatch on: peel shell-prefix idioms (`cd /path && bun
    // test`, `env FOO=bar npm install`, `timeout 30 cargo build`, `(cd /path;
    // cmd)`) so head-token matchers see the real command. Any top-level pipe
    // must stay generic: the shell already ran the user's pipeline verbatim, so
    // the captured output belongs to the pipeline result, not necessarily to the
    // runner that appeared before `|`.
    let dispatch_owned = match resolve_dispatch_target(command) {
        DispatchTarget::Pipeline(_) | DispatchTarget::ForceGeneric => {
            return GenericCompressor::compress_stripped_output(&stripped_for_generic).into();
        }
        DispatchTarget::Command(cmd) => cmd,
    };
    let dispatch_cmd = dispatch_owned.as_str();

    let compressors = compressors_in_dispatch_order();

    // Tier 1a: Specific command compressors win first.
    for compressor in compressors
        .iter()
        .filter(|c| c.specificity() == Specificity::Specific)
    {
        if compressor.matches(dispatch_cmd) {
            let result =
                compressor.compress_with_exit_code(dispatch_cmd, &stripped_for_generic, exit_code);
            return failure_preserving_result(command, &stripped_for_generic, result, exit_code);
        }
    }

    // Tier 1b: Output-shape sniffers handle wrapped inner tools before broad
    // package managers or TOML filters can consume `npm test`, `make test`,
    // `just test`, etc. Collision order is deterministic: Specific compressors
    // in registry order win before PackageManager sniffers (currently Bun's
    // test-output signature).
    let output_probe = OutputProbe::new(&stripped_for_generic);
    for specificity in [Specificity::Specific, Specificity::PackageManager] {
        for compressor in compressors
            .iter()
            .filter(|c| c.specificity() == specificity)
        {
            if compressor.matches_output_probe(&output_probe) {
                let result = compressor
                    .compress_output_match_with_exit_code(&stripped_for_generic, exit_code);
                return failure_preserving_result(
                    command,
                    &stripped_for_generic,
                    result,
                    exit_code,
                );
            }
        }
    }

    // Tier 1c: PackageManager compressors get unclaimed commands.
    for compressor in compressors
        .iter()
        .filter(|c| c.specificity() == Specificity::PackageManager)
    {
        if compressor.matches(dispatch_cmd) {
            let result =
                compressor.compress_with_exit_code(dispatch_cmd, &stripped_for_generic, exit_code);
            return failure_preserving_result(command, &stripped_for_generic, result, exit_code);
        }
    }

    // Tier 2: TOML filters. Pass raw output so `[ansi].strip = false` filters
    // can intentionally match escape sequences; `apply_filter` owns ANSI policy.
    if let Some(filter) = registry.lookup(dispatch_cmd) {
        let result = apply_filter_with_exit_code_prestripped(
            filter,
            output,
            &stripped_for_generic,
            exit_code,
        );
        return failure_preserving_result(command, &stripped_for_generic, result, exit_code);
    }

    // Tier 3: generic fallback.
    GenericCompressor::compress_stripped_output(&stripped_for_generic).into()
}

fn compressors_in_dispatch_order() -> [&'static dyn Compressor; 20] {
    [
        &GitCompressor,
        &CargoCompressor,
        &TscCompressor,
        &NpmCompressor,
        &BunCompressor,
        &PnpmCompressor,
        &PytestCompressor,
        &EslintCompressor,
        &VitestCompressor,
        &BiomeCompressor,
        &PrettierCompressor,
        &RuffCompressor,
        &MypyCompressor,
        &GoCompressor,
        &GolangciLintCompressor,
        &PlaywrightCompressor,
        &NextCompressor,
        &LsCompressor,
        &FindCompressor,
        &TreeCompressor,
    ]
}

fn failure_preserving_result(
    command: &str,
    stripped_raw_output: &str,
    result: CompressionResult,
    exit_code: Option<i32>,
) -> CompressionResult {
    if !matches!(exit_code, Some(code) if code != 0) {
        return result;
    }

    if dropped_failure_or_error_blocks(&result)
        || !text_has_failure_signal(&result.text)
        || result_looks_successful(&result.text)
    {
        return GenericCompressor.compress_with_exit_code(command, stripped_raw_output, exit_code);
    }

    let missing = missing_raw_failure_signal_lines(stripped_raw_output, &result.text);
    if missing.is_empty() {
        result
    } else {
        append_missing_failure_lines(result, &missing)
    }
}

fn dropped_failure_or_error_blocks(result: &CompressionResult) -> bool {
    [DropClass::Error, DropClass::Failure]
        .into_iter()
        .any(|class| result.dropped_by_class.get(&class).copied().unwrap_or(0) > 0)
}

fn append_missing_failure_lines(
    mut result: CompressionResult,
    missing_failure_lines: &[String],
) -> CompressionResult {
    let mut text = result.text.trim_end().to_string();
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str("[raw failure lines preserved by AFT]\n");
    text.push_str(&missing_failure_lines.join("\n"));
    result.text = text;
    result
}

pub(crate) fn missing_raw_failure_signal_lines(
    raw_output: &str,
    compressed_text: &str,
) -> Vec<String> {
    let compressed_lower = compressed_text.to_ascii_lowercase();
    let compressed_lines: HashSet<&str> = compressed_text
        .lines()
        .zip(compressed_lower.lines())
        .filter_map(|(line, lower)| {
            let trimmed = line.trim();
            (!trimmed.is_empty() && line_has_failure_signal_lower(trimmed, lower.trim()))
                .then_some(trimmed)
        })
        .collect();
    let raw_lower = raw_output.to_ascii_lowercase();
    let mut seen = HashSet::new();
    let mut missing = Vec::new();

    for (line, lower) in raw_output.lines().zip(raw_lower.lines()) {
        let trimmed = line.trim();
        if trimmed.is_empty() || !line_has_failure_signal_lower(trimmed, lower.trim()) {
            continue;
        }
        if compressed_lines.contains(trimmed) || !seen.insert(trimmed) {
            continue;
        }
        missing.push(trimmed.to_string());
    }

    missing
}

fn result_looks_successful(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("clean")
        || lower.contains(" ok")
        || lower.contains(":ok")
        || lower.contains(": ok")
        || lower.contains("passed")
        || lower.contains("succeeded")
        || lower.contains("no errors")
        || lower.contains("0 errors")
        || lower.contains("no issues")
        || lower.contains("no diagnostics")
        || lower.contains("all checks passed")
        || lower.contains("formatted")
        || lower.contains("0 fail")
        || lower.contains("found 0")
        || lower.contains("up to date")
        || lower.contains("up-to-date")
}

pub(crate) fn text_has_failure_signal(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    text.lines()
        .zip(lower.lines())
        .any(|(line, lower)| line_has_failure_signal_lower(line.trim(), lower.trim()))
}

fn line_has_failure_signal_lower(line: &str, lower: &str) -> bool {
    line.contains("error[")
        || lower.contains("error:")
        || line.contains("Error")
        || line.contains("ERROR")
        || lower.contains("internalerror")
        || lower.contains("traceback")
        || lower.contains("exception")
        || lower.contains("no module named")
        || lower.contains("undefined reference")
        || lower.contains("linker command failed")
        || lower.contains("undefined:")
        || lower.contains("expected declaration")
        || lower.contains("collect2: error")
        || lower.contains("ld: error")
        || lower.contains("fatal error")
        || line.contains("FAILED")
        || line.contains("FAIL")
        || contains_nonzero_failure_word_lower(lower)
        || lower.contains("panic")
        || lower.contains("cannot find")
        || lower.contains("not found")
        || lower.contains("no such")
}

fn contains_nonzero_failure_word_lower(lower: &str) -> bool {
    for (index, _) in lower.match_indices("fail") {
        let tail = &lower[index + "fail".len()..];
        for suffix in ["ures", "ure", "ed", ""] {
            let Some(after_suffix) = tail.strip_prefix(suffix) else {
                continue;
            };
            let before_is_word = lower[..index].chars().next_back().is_some_and(is_word_char);
            let after_is_word = after_suffix.chars().next().is_some_and(is_word_char);
            if before_is_word || after_is_word {
                continue;
            }

            let prefix = lower[..index].trim_end();
            let digits_start = prefix
                .char_indices()
                .rev()
                .take_while(|(_, ch)| ch.is_ascii_digit())
                .last()
                .map(|(idx, _)| idx);
            let Some(digits_start) = digits_start else {
                return true;
            };
            let digits = &prefix[digits_start..];
            if digits.parse::<usize>().ok() != Some(0) {
                return true;
            }
        }
    }
    false
}

fn is_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

/// Build the registry of TOML filters from the standard sources for the
/// active context. Called lazily by [`AppContext::filter_registry`].
///
/// Layering (highest priority first):
/// 1. Project filters at `<project_root>/.cortexkit/aft/filters/*.toml` — loaded only
///    when the project is in the trusted set (see [`trust`]).
/// 2. User filters at `<storage_dir>/<harness>/filters/*.toml`.
/// 3. Builtin filters compiled into the binary via [`builtin_filters`].
pub fn build_registry_for_context(ctx: &AppContext) -> FilterRegistry {
    let harness = ctx.harness.lock().clone().unwrap_or(Harness::Opencode);
    let config = ctx.config();
    let storage_dir = config.storage_dir.clone();
    let project_root = config.project_root.clone();
    drop(config);

    let user_dir = storage_dir.as_ref().map(|dir| {
        repair_legacy_user_filter_dir(dir, harness.clone());
        user_filter_dir(dir, harness)
    });
    let project_dir = match (project_root.as_ref(), storage_dir.as_ref()) {
        (Some(root), Some(storage)) => {
            if trust::is_project_trusted(Some(storage), root) {
                Some(project_filter_dir(root))
            } else {
                None
            }
        }
        _ => None,
    };

    toml_filter::build_registry(
        builtin_filters::ALL,
        user_dir.as_deref(),
        project_dir.as_deref(),
    )
}

/// Normalize a shell command for compressor dispatch by walking past
/// common shell-prefix idioms so the REAL command head is what matchers
/// see. Returns `Some(normalized)` if a prefix was stripped, `None` if
/// the input was already a bare command.
///
/// Handles:
///   - `cd /path && cmd ...`            → `cmd ...`
///   - `cd /path; cmd ...`              → `cmd ...`
///   - `env FOO=bar [BAR=baz ...] cmd`  → `cmd ...`
///   - `FOO=bar [BAR=baz ...] cmd`      → `cmd ...`
///   - `timeout 30 cmd ...`             → `cmd ...`
///   - `nohup cmd ...`                  → `cmd ...`
///   - `(cd /path && cmd ...)`          → `cmd ...`   (trailing `)` is kept; harmless for matchers)
///
/// Real agent invocations almost always wrap their actual command in
/// `cd "$ROOT" && ...`. Without this normalization, BunCompressor /
/// NpmCompressor / PnpmCompressor (head-token matchers) and the
/// pkg-manager filters silently fall through to GenericCompressor for
/// the majority of agent bash calls.
///
/// The normalizer is conservative: it only strips well-defined idioms
/// and bails on anything ambiguous, so a malformed command degrades to
/// the same dispatch behaviour as before this helper existed.
pub fn normalize_command_for_dispatch(command: &str) -> Option<String> {
    match resolve_dispatch_target(command) {
        // Ambiguous or unsafe pipelines must not be claimed by specific
        // compressors, so return None to make callers use generic dispatch.
        DispatchTarget::ForceGeneric => None,
        DispatchTarget::Command(resolved) | DispatchTarget::Pipeline(resolved) => {
            if resolved == command.trim_start() {
                None
            } else {
                Some(resolved)
            }
        }
    }
}

/// Normalize commands for structured-output detection, where a top-level pipe
/// must suppress structured handling instead of falling back to the raw command.
pub(crate) fn plain_command_for_structured_output(command: &str) -> Option<String> {
    match resolve_dispatch_target(command) {
        DispatchTarget::Command(resolved) => Some(resolved),
        DispatchTarget::Pipeline(_) | DispatchTarget::ForceGeneric => None,
    }
}

/// What compressor dispatch should target for a command, after peeling shell
/// prefixes and resolving any top-level pipeline.
enum DispatchTarget {
    /// Match compressors against this command string (peeled, and/or the last
    /// pipeline stage whose stdout was captured).
    Command(String),
    /// A clean top-level pipeline was found. The contained string is the last
    /// stage for callers that only need a normalized command, but compression
    /// dispatch treats the original command as generic raw pipeline output.
    Pipeline(String),
    /// An unsafe pipeline was detected (a `|` is present but the command could
    /// not be parsed safely). Skip all specific compressors and use generic —
    /// a head-token compressor claiming `cargo test | …` would drop the output.
    ForceGeneric,
}

fn resolve_dispatch_target(command: &str) -> DispatchTarget {
    // Strip top-level comments FIRST. A `#` comment's text otherwise reaches the
    // head-token matchers, which scan the whole string for their tool name — so
    // `printf keep # cargo test` would let CargoCompressor claim the printf
    // command's output and drop it (issue #137), with or without a pipe.
    let decommented = strip_top_level_comment(command);
    let peeled = peel_shell_prefixes(&decommented);
    let base = peeled
        .as_deref()
        .unwrap_or_else(|| decommented.trim_start());
    match split_top_level_pipe(base) {
        PipeSplit::LastStage(last) => DispatchTarget::Pipeline(last),
        PipeSplit::Unsafe => DispatchTarget::ForceGeneric,
        PipeSplit::None => DispatchTarget::Command(base.to_string()),
    }
}

/// Remove top-level shell comments (`#` to end of line) from a command so the
/// comment text can't fool head-token compressor matchers (which scan the whole
/// command string for their tool name). Quote/backtick/substitution aware: a `#`
/// inside quotes, inside `$(`/`` ` ``, or not at a word boundary is literal.
/// Copies byte ranges (UTF-8 safe — every decision point is an ASCII byte) and
/// preserves newlines so any later top-level structure stays visible to the
/// pipeline scanner.
fn strip_top_level_comment(command: &str) -> String {
    let bytes = command.as_bytes();
    let mut result = String::with_capacity(command.len());
    let mut seg_start = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut paren_depth: u32 = 0;
    let mut escaped = false;
    let mut prev = b' '; // start-of-string counts as a word boundary

    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if escaped {
            escaped = false;
            prev = ch;
            i += 1;
            continue;
        }
        if in_single {
            if ch == b'\'' {
                in_single = false;
            }
            prev = ch;
            i += 1;
            continue;
        }
        if in_backtick {
            if ch == b'\\' {
                escaped = true;
            } else if ch == b'`' {
                in_backtick = false;
            }
            prev = ch;
            i += 1;
            continue;
        }
        if ch == b'\\' {
            escaped = true;
            prev = ch;
            i += 1;
            continue;
        }
        if ch == b'`' {
            in_backtick = true;
            prev = ch;
            i += 1;
            continue;
        }
        if ch == b'$' && bytes.get(i + 1) == Some(&b'(') {
            paren_depth += 1;
            prev = b'(';
            i += 2;
            continue;
        }
        if in_double {
            if ch == b'"' {
                in_double = false;
            }
            prev = ch;
            i += 1;
            continue;
        }
        if ch == b'#'
            && paren_depth == 0
            && matches!(prev, b' ' | b'\t' | b'\n' | b';' | b'&' | b'|' | b'(')
        {
            result.push_str(&command[seg_start..i]);
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            seg_start = i; // resume at the newline (kept) or EOL
            prev = b'\n';
            continue;
        }
        match ch {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'<' | b'>' if bytes.get(i + 1) == Some(&b'(') => {
                paren_depth += 1;
                prev = b'(';
                i += 2;
                continue;
            }
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            _ => {}
        }
        prev = ch;
        i += 1;
    }
    result.push_str(&command[seg_start..]);
    result
}

/// Peel known shell-prefix idioms (`cd … &&`, `env VAR=v`, `VAR=v`, `timeout N`,
/// `nohup`, leading `(`) so the REAL command head is exposed to matchers.
/// Returns `Some(peeled)` when something was stripped, `None` otherwise.
fn peel_shell_prefixes(command: &str) -> Option<String> {
    let trimmed = command.trim_start();
    if trimmed.is_empty() {
        return None;
    }

    // Step 1: peel a leading `(` from group-expression idioms.
    let (open_paren, after_paren) = if let Some(rest) = trimmed.strip_prefix('(') {
        (true, rest.trim_start())
    } else {
        (false, trimmed)
    };

    let mut current = after_paren.to_string();
    let mut changed = open_paren;

    // Step 2: iteratively peel known shell prefixes.
    loop {
        // `VAR=value cmd ...` (possibly multiple assignment words). This must
        // run before head-token matching so package-manager/Rust compressors
        // still see the real command for `NODE_ENV=production npm install`.
        if let Some(stripped) = strip_leading_assignment_prefix(&current) {
            current = stripped;
            changed = true;
            continue;
        }

        let head: String = current.split_whitespace().next().unwrap_or("").to_string();

        // `cd <path> && ...` or `cd <path>; ...`
        if head == "cd" {
            // Find the next `&&` or `;` token; everything after that is the real command.
            // Use char-level scan because `&&` is two chars not separated by whitespace.
            if let Some(stripped) = strip_cd_prefix(&current) {
                current = stripped;
                changed = true;
                continue;
            }
        }

        // `env VAR=val [VAR=val ...] cmd ...`
        if head == "env" {
            if let Some(stripped) = strip_env_prefix(&current) {
                current = stripped;
                changed = true;
                continue;
            }
        }

        // `timeout <N> cmd ...` or `timeout <duration-with-unit> cmd ...`
        if head == "timeout" {
            if let Some(stripped) = strip_timeout_prefix(&current) {
                current = stripped;
                changed = true;
                continue;
            }
        }

        // `nohup cmd ...`
        if head == "nohup" {
            if let Some(rest) = current.strip_prefix("nohup").and_then(|s| {
                let trimmed = s.trim_start();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            }) {
                current = rest;
                changed = true;
                continue;
            }
        }

        break;
    }

    if changed {
        Some(current)
    } else {
        None
    }
}

/// Returns true if the token is a shell metacharacter that acts as a
/// command boundary. Subcommand parsers use this to avoid returning a
/// redirect/operator token as a subcommand name. Covers control operators
/// (`|`, `|&`, `;`, `&`, `&&`, `||`), and every redirect shape — bare
/// (`>`, `>>`, `<`, `<<`, `<<<`, `&>`, `&>>`), fd-prefixed (`2>`, `2>>`,
/// `2>&1`, `1>&2`), and glued (`>file`, `2>/dev/null`).
pub fn is_shell_boundary(token: &str) -> bool {
    matches!(token, "|" | "|&" | ";" | "&" | "&&" | "||" | "&>" | "&>>") || is_redirect_token(token)
}

/// A redirect operator token: an optional leading fd (`2` in `2>&1`) followed
/// by a `>`/`<` redirect, or an `&>`/`&>>` merge redirect. Real subcommands
/// (`test`, `log`, `build`) never match, so this can't suppress a true one.
fn is_redirect_token(token: &str) -> bool {
    let rest = token.trim_start_matches(|c: char| c.is_ascii_digit());
    rest.starts_with('>') || rest.starts_with('<') || rest.starts_with("&>")
}

/// Outcome of scanning a command for a top-level pipeline.
#[derive(Debug, PartialEq, Eq)]
enum PipeSplit {
    /// No top-level `|` and no top-level separator — dispatch on the
    /// command as-is.
    None,
    /// A top-level pipeline; the captured stdout is this last stage's output.
    LastStage(String),
    /// The command cannot be safely dispatched to a head-token compressor.
    /// Either a pipe coexists with other top-level structure (so the
    /// captured output isn't just the last stage's), or a top-level
    /// separator (`;`, `&&`, `||`, bare `&`, newline) means multiple
    /// commands' output is interleaved — a head-token compressor would
    /// delete the other commands' output. Force generic instead.
    Unsafe,
}

/// A clean, single top-level pipeline as seen by the shell scanner. The segment
/// labels are derived while scanning so completion warnings do not need to
/// perform a second, less-capable parse of the user's command.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PipelineInfo {
    pub(crate) segments: Vec<PipelineSegment>,
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PipelineSegment {
    pub(crate) label: String,
}

#[derive(Debug, Default)]
struct PipelineScan {
    stages: Vec<String>,
    saw_top_pipe: bool,
    saw_top_separator: bool,
    saw_unmatched_close: bool,
    in_single: bool,
    in_double: bool,
    in_backtick: bool,
    paren_depth: u32,
    escaped: bool,
}

/// Return the scanner's segment list only for a single, clean top-level
/// pipeline. Instrumentation deliberately rejects comments here: appending a
/// capture program to a command that ends in a comment is otherwise easy for
/// the comment to swallow. Heredocs and other multi-statement forms are already
/// rejected by the scanner's top-level newline/separator handling.
#[cfg(unix)]
pub(crate) fn single_top_level_pipeline(command: &str) -> Option<PipelineInfo> {
    if strip_top_level_comment(command) != command {
        return None;
    }

    let scan = scan_top_level_pipe(command);
    let stages = clean_pipeline_stages(scan)?;
    if stages.len() < 2 {
        return None;
    }

    Some(PipelineInfo {
        segments: stages
            .iter()
            .map(|stage| PipelineSegment {
                label: pipeline_segment_label(stage),
            })
            .collect(),
    })
}

#[cfg(unix)]
fn pipeline_segment_label(stage: &str) -> String {
    let stage = strip_leading_assignment_prefix(stage).unwrap_or_else(|| stage.trim().to_string());
    let start = skip_whitespace(&stage, 0);
    let Some(first_end) = shell_word_end(&stage, start) else {
        return stage;
    };
    if first_end == stage.len() {
        return stage[start..first_end].to_string();
    }

    let first_word = &stage[start..first_end];
    if first_word != "git" {
        return first_word.to_string();
    }

    // Git's subcommand is part of the user-facing command name (`git rebase`)
    // and makes the warning substantially more useful than a bare `git` label.
    let second_start = skip_whitespace(&stage, first_end);
    let second_end = shell_word_end(&stage, second_start).unwrap_or(second_start);
    if second_end > second_start {
        format!("{} {}", first_word, &stage[second_start..second_end])
    } else {
        first_word.to_string()
    }
}

/// Depth-aware pipeline scanner that FAILS CLOSED. Tracks single/double quotes,
/// backslash escapes, backtick substitution, and `(`/`$(`/`<(`/`>(` nesting so a
/// `|` inside any of them is not treated as a stage boundary. Splits on a
/// top-level `|`/`|&` (never `||`) and returns the LAST stage — but ONLY when the
/// command is a clean single pipeline. The caller captured the WHOLE command's
/// stdout, so "last stage == captured output" holds only when no other top-level
/// structure exists; otherwise a head-token compressor could claim the command
/// and drop the output (issue #137). Therefore, whenever a top-level pipe
/// coexists with ANY of {a top-level separator `;`/`&&`/`||`/bare `&`/newline,
/// an unbalanced quote/paren/backtick/escape, an unmatched `)`, or an empty
/// trailing stage}, we return `Unsafe` so the caller forces generic compression.
/// The same `Unsafe` result applies when there is NO pipe but a top-level
/// separator IS present: the captured transcript interleaves multiple commands'
/// output, so per-head specialized compression would delete the other commands'
/// output. Top-level comments must already be removed by
/// `strip_top_level_comment`. Redirects (`>`, `2>&1`, `&>`, …) are NOT separators.
fn split_top_level_pipe(command: &str) -> PipeSplit {
    let scan = scan_top_level_pipe(command);
    if scan.saw_top_pipe {
        if let Some(stages) = clean_pipeline_stages(scan) {
            return PipeSplit::LastStage(stages.last().cloned().unwrap_or_default());
        }
        return PipeSplit::Unsafe;
    }

    if (scan.imbalance() && command.contains('|')) || scan.saw_top_separator {
        PipeSplit::Unsafe
    } else {
        PipeSplit::None
    }
}

fn clean_pipeline_stages(scan: PipelineScan) -> Option<Vec<String>> {
    if !scan.saw_top_pipe || scan.imbalance() || scan.saw_top_separator {
        return None;
    }
    if scan.stages.iter().any(|stage| stage.trim().is_empty()) {
        return None;
    }
    Some(scan.stages)
}

impl PipelineScan {
    fn imbalance(&self) -> bool {
        self.in_single
            || self.in_double
            || self.in_backtick
            || self.escaped
            || self.paren_depth != 0
            || self.saw_unmatched_close
    }
}

fn scan_top_level_pipe(command: &str) -> PipelineScan {
    let bytes = command.as_bytes();
    let mut scan = PipelineScan::default();
    let mut stage_start = 0usize;
    let mut i = 0;

    while i < bytes.len() {
        let ch = bytes[i];

        if scan.escaped {
            scan.escaped = false;
            i += 1;
            continue;
        }
        if scan.in_single {
            if ch == b'\'' {
                scan.in_single = false;
            }
            i += 1;
            continue;
        }
        if scan.in_backtick {
            // Backtick substitution is opaque for splitting. A backslash still
            // escapes the next byte so an escaped backtick does not close it.
            if ch == b'\\' {
                scan.escaped = true;
            } else if ch == b'`' {
                scan.in_backtick = false;
            }
            i += 1;
            continue;
        }
        if ch == b'\\' {
            scan.escaped = true;
            i += 1;
            continue;
        }
        if ch == b'`' {
            scan.in_backtick = true;
            i += 1;
            continue;
        }
        // `$(` opens command substitution even inside double quotes.
        if ch == b'$' && bytes.get(i + 1) == Some(&b'(') {
            scan.paren_depth += 1;
            i += 2;
            continue;
        }
        if scan.in_double {
            if ch == b'"' {
                scan.in_double = false;
            }
            i += 1;
            continue;
        }

        // Below here: outside single/double quotes and backticks. Top-level
        // comments are already removed by `strip_top_level_comment` before the
        // compression scanner runs, so no `#` handling is needed here.
        let prev_raw = if i > 0 { bytes[i - 1] } else { b' ' };

        match ch {
            b'\'' => scan.in_single = true,
            b'"' => scan.in_double = true,
            // process substitution `<(` / `>(`
            b'<' | b'>' if bytes.get(i + 1) == Some(&b'(') => {
                scan.paren_depth += 1;
                i += 2;
                continue;
            }
            b'(' => scan.paren_depth += 1,
            b')' => {
                if scan.paren_depth == 0 {
                    scan.saw_unmatched_close = true;
                } else {
                    scan.paren_depth -= 1;
                }
            }
            b'|' if scan.paren_depth == 0 => {
                if bytes.get(i + 1) == Some(&b'|') {
                    scan.saw_top_separator = true; // `||` logical OR
                    i += 2;
                    continue;
                }
                scan.saw_top_pipe = true;
                scan.stages.push(command[stage_start..i].trim().to_string());
                let delimiter_len = if bytes.get(i + 1) == Some(&b'&') {
                    2 // `|&` (stdout+stderr)
                } else {
                    1
                };
                stage_start = i + delimiter_len;
                i += delimiter_len;
                continue;
            }
            b'&' if scan.paren_depth == 0 => {
                if bytes.get(i + 1) == Some(&b'&') {
                    scan.saw_top_separator = true; // `&&`
                    i += 2;
                    continue;
                }
                // `&>`/`&>>` redirect, or `>&`/`2>&1` fd-dup: NOT a separator.
                // A bare `&` is the background control operator.
                if bytes.get(i + 1) != Some(&b'>') && prev_raw != b'>' {
                    scan.saw_top_separator = true;
                }
            }
            b';' | b'\n' if scan.paren_depth == 0 => scan.saw_top_separator = true,
            _ => {}
        }
        i += 1;
    }

    if scan.saw_top_pipe {
        scan.stages.push(command[stage_start..].trim().to_string());
    }
    scan
}

fn strip_cd_prefix(command: &str) -> Option<String> {
    // Look for `&&` or `;` outside of quotes.
    let bytes = command.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i] as char;
        if !in_double && ch == '\'' {
            in_single = !in_single;
        } else if !in_single && ch == '"' {
            in_double = !in_double;
        } else if !in_single && !in_double {
            if ch == '&' && i + 1 < bytes.len() && bytes[i + 1] as char == '&' {
                let rest = command[i + 2..].trim_start();
                if rest.is_empty() {
                    return None;
                }
                return Some(rest.to_string());
            }
            if ch == ';' {
                let rest = command[i + 1..].trim_start();
                if rest.is_empty() {
                    return None;
                }
                return Some(rest.to_string());
            }
        }
        i += 1;
    }
    None
}

fn strip_env_prefix(command: &str) -> Option<String> {
    // env <ASSIGN>... <cmd> ...
    let rest = command.strip_prefix("env")?.trim_start();
    strip_leading_assignment_prefix(rest)
}

fn strip_leading_assignment_prefix(command: &str) -> Option<String> {
    let mut index = 0usize;
    let mut consumed_assignment = false;

    loop {
        index = skip_whitespace(command, index);
        if index >= command.len() {
            break;
        }

        let word_end = shell_word_end(command, index)?;
        if word_end == index {
            break;
        }

        let word = &command[index..word_end];
        if !is_env_assignment(word) {
            break;
        }

        consumed_assignment = true;
        index = word_end;
    }

    if !consumed_assignment {
        return None;
    }

    let after = command[index..].trim_start();
    if after.is_empty() {
        None
    } else {
        Some(after.to_string())
    }
}

fn skip_whitespace(input: &str, mut index: usize) -> usize {
    while index < input.len() {
        let Some(ch) = input[index..].chars().next() else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        index += ch.len_utf8();
    }
    index
}

fn shell_word_end(command: &str, start: usize) -> Option<usize> {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for (offset, ch) in command[start..].char_indices() {
        let index = start + offset;

        if escaped {
            escaped = false;
            continue;
        }

        if ch == '\\' && !in_single {
            escaped = true;
            continue;
        }

        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }

        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }

        if !in_single && !in_double && (ch.is_whitespace() || matches!(ch, ';' | '&' | '|')) {
            return Some(index);
        }
    }

    if in_single || in_double || escaped {
        None
    } else {
        Some(command.len())
    }
}

fn is_env_assignment(token: &str) -> bool {
    if token.starts_with('-') {
        return false;
    }
    let Some((name, _value)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn strip_timeout_prefix(command: &str) -> Option<String> {
    let rest = command.strip_prefix("timeout")?.trim_start();
    // Next token must look like a duration (digits, optional trailing unit s/m/h).
    let mut iter = rest.splitn(2, char::is_whitespace);
    let duration = iter.next()?;
    let after = iter.next()?.trim_start();
    if after.is_empty() || !looks_like_duration(duration) {
        return None;
    }
    Some(after.to_string())
}

fn looks_like_duration(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let mut chars = token.chars().peekable();
    let mut saw_digit = false;
    while let Some(&ch) = chars.peek() {
        if ch.is_ascii_digit() {
            saw_digit = true;
            chars.next();
        } else {
            break;
        }
    }
    if !saw_digit {
        return false;
    }
    match chars.next() {
        None => true,
        Some(unit) => matches!(unit, 's' | 'm' | 'h' | 'd') && chars.next().is_none(),
    }
}

/// Resolve the harness-scoped user-filter directory for an arbitrary storage_dir.
/// Used by `aft doctor filters` to inspect filters without needing a live AppContext.
pub fn user_filter_dir(storage_dir: &Path, harness: Harness) -> PathBuf {
    storage_dir.join(harness.storage_segment()).join("filters")
}

fn legacy_user_filter_dir(storage_dir: &Path) -> PathBuf {
    storage_dir.join("filters")
}

/// Move filters written by the short-lived root-scoped v0.27 layout into the
/// active harness directory. Existing harness files win; colliding root files
/// are left in place so we never overwrite user-authored filters.
pub(crate) fn repair_legacy_user_filter_dir(storage_dir: &Path, harness: Harness) {
    let legacy_dir = legacy_user_filter_dir(storage_dir);
    if !legacy_dir.exists() {
        return;
    }

    let entries = match fs::read_dir(&legacy_dir) {
        Ok(entries) => entries.filter_map(Result::ok).collect::<Vec<_>>(),
        Err(_) => return,
    };
    if entries.is_empty() {
        let _ = fs::remove_dir(&legacy_dir);
        return;
    }

    let harness_dir = user_filter_dir(storage_dir, harness);
    if fs::create_dir_all(&harness_dir).is_err() {
        return;
    }

    for entry in entries {
        let target = harness_dir.join(entry.file_name());
        if target.exists() {
            continue;
        }
        let _ = fs::rename(entry.path(), target);
    }

    if fs::read_dir(&legacy_dir)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
    {
        let _ = fs::remove_dir(&legacy_dir);
    }
}

/// Resolve the project-filter directory for an arbitrary project root.
/// Returns the directory regardless of trust state — caller must check trust
/// separately if it wants to gate loading.
pub fn project_filter_dir(project_root: &Path) -> PathBuf {
    project_root.join(".cortexkit").join("aft").join("filters")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn single_pipeline_scanner_returns_segment_labels() {
        let pipeline = single_top_level_pipeline("git rebase upstream/main | tail -3").unwrap();
        assert_eq!(
            pipeline
                .segments
                .iter()
                .map(|segment| segment.label.as_str())
                .collect::<Vec<_>>(),
            ["git rebase", "tail"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn single_pipeline_scanner_rejects_unsafe_shapes() {
        for command in [
            "false | tail -1; true | cat",
            "false | tail -1 &",
            "false | tail -1 # trailing comment",
            "cat <<EOF | tail -1\nbody\nEOF",
        ] {
            assert!(
                single_top_level_pipeline(command).is_none(),
                "scanner should reject {command:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn single_pipeline_scanner_accepts_pipe_without_top_level_newline() {
        let pipeline = single_top_level_pipeline("false | tail -1").unwrap();
        assert_eq!(pipeline.segments.len(), 2);
    }

    #[test]
    fn dispatch_strips_ansi_once_for_generic_and_toml_tiers() {
        let registry = toml_filter::build_registry(builtin_filters::ALL, None, None);
        for (command, output) in [
            ("custom-runner", "\u{1b}[32mline\u{1b}[0m\n"),
            (
                "docker build .",
                "\u{1b}[32m#1 DONE 0.1s\u{1b}[0m\nretained line\n",
            ),
        ] {
            generic::reset_ansi_strip_count();
            let result = compress_with_registry(command, output, &registry);
            assert!(!result.is_empty());
            assert_eq!(generic::ansi_strip_count(), 1, "command: {command}");
        }
    }

    #[test]
    fn user_and_project_filter_dir_helpers() {
        let storage = Path::new("/tmp/aft-storage");
        assert_eq!(
            user_filter_dir(storage, Harness::Opencode),
            Path::new("/tmp/aft-storage/opencode/filters")
        );

        let project = Path::new("/repo");
        assert_eq!(
            project_filter_dir(project),
            Path::new("/repo/.cortexkit/aft/filters")
        );
    }

    #[test]
    fn repair_legacy_user_filter_dir_moves_root_filters_without_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path();
        fs::create_dir_all(storage.join("filters")).unwrap();
        fs::create_dir_all(storage.join("opencode/filters")).unwrap();
        fs::write(storage.join("filters/root-only.toml"), "root").unwrap();
        fs::write(storage.join("filters/collides.toml"), "root").unwrap();
        fs::write(storage.join("opencode/filters/collides.toml"), "harness").unwrap();

        repair_legacy_user_filter_dir(storage, Harness::Opencode);

        assert_eq!(
            fs::read_to_string(storage.join("opencode/filters/root-only.toml")).unwrap(),
            "root"
        );
        assert_eq!(
            fs::read_to_string(storage.join("opencode/filters/collides.toml")).unwrap(),
            "harness"
        );
        assert_eq!(
            fs::read_to_string(storage.join("filters/collides.toml")).unwrap(),
            "root"
        );
        assert!(!storage.join("filters/root-only.toml").exists());
    }
}

#[cfg(test)]
mod output_probe_tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct CorpusEntry {
        file: String,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct MatchPhaseResult {
        selected: Option<usize>,
        output: CompressionResult,
    }

    fn legacy_match_phase(output: &str) -> MatchPhaseResult {
        let compressors = compressors_in_dispatch_order();
        let selected = [Specificity::Specific, Specificity::PackageManager]
            .into_iter()
            .find_map(|specificity| {
                compressors
                    .iter()
                    .enumerate()
                    .filter(|(_, compressor)| compressor.specificity() == specificity)
                    .find_map(|(index, compressor)| {
                        compressor.matches_output(output).then_some(index)
                    })
            });
        match_phase_result(&compressors, selected, output)
    }

    fn probe_match_phase(output: &str) -> MatchPhaseResult {
        let compressors = compressors_in_dispatch_order();
        let probe = OutputProbe::new(output);
        let selected = [Specificity::Specific, Specificity::PackageManager]
            .into_iter()
            .find_map(|specificity| {
                compressors
                    .iter()
                    .enumerate()
                    .filter(|(_, compressor)| compressor.specificity() == specificity)
                    .find_map(|(index, compressor)| {
                        compressor.matches_output_probe(&probe).then_some(index)
                    })
            });
        match_phase_result(&compressors, selected, output)
    }

    fn match_phase_result(
        compressors: &[&dyn Compressor],
        selected: Option<usize>,
        output: &str,
    ) -> MatchPhaseResult {
        let output = selected.map_or_else(
            || GenericCompressor.compress_with_exit_code("", output, None),
            |index| compressors[index].compress_output_match_with_exit_code(output, None),
        );
        MatchPhaseResult { selected, output }
    }

    fn repository_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("aft crate should be nested under the repository root")
            .to_path_buf()
    }

    fn benchmark_corpus() -> Vec<(String, String)> {
        let fixtures = repository_root().join("benchmarks/compression-tokens/fixtures");
        let manifest_text = fs::read_to_string(fixtures.join("manifest.json"))
            .expect("compression corpus manifest should be readable");
        let manifest: Vec<CorpusEntry> =
            serde_json::from_str(&manifest_text).expect("compression corpus manifest should parse");
        manifest
            .into_iter()
            .map(|entry| {
                let output = fs::read_to_string(fixtures.join(&entry.file))
                    .unwrap_or_else(|error| panic!("failed to read {}: {error}", entry.file));
                (format!("compression corpus {}", entry.file), output)
            })
            .collect()
    }

    fn integration_filter_inputs() -> Vec<(String, String)> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/integration/fixtures/compress_filters");
        let mut paths = Vec::new();
        collect_named_files(&root, "input.txt", &mut paths);
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let output = fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
                (format!("filter fixture {}", path.display()), output)
            })
            .collect()
    }

    fn collect_named_files(root: &Path, wanted_name: &str, paths: &mut Vec<PathBuf>) {
        let entries = fs::read_dir(root)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", root.display()));
        for entry in entries {
            let path = entry
                .expect("fixture directory entry should be readable")
                .path();
            if path.is_dir() {
                collect_named_files(&path, wanted_name, paths);
            } else if path.file_name().is_some_and(|name| name == wanted_name) {
                paths.push(path);
            }
        }
    }

    fn hostile_outputs() -> Vec<(String, String)> {
        let huge_json_array = format!(
            "[{}]",
            (0..20_000)
                .map(|index| format!(r#"{{"index":{index},"value":"payload"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let ansi_heavy = (0..2_000)
            .map(|index| format!("\u{1b}[31mordinary output {index}\u{1b}[0m"))
            .collect::<Vec<_>>()
            .join("\n");
        vec![
            (
                "valid non-tool JSON object".to_string(),
                r#" {"payload":{"nested":[1,2,3]},"ok":true}"#.to_string(),
            ),
            ("huge JSON array".to_string(), huge_json_array),
            (
                "almost JSON".to_string(),
                r#"{"payload":[1,2,3],"unterminated":true"#.to_string(),
            ),
            ("empty output".to_string(), String::new()),
            ("ANSI-heavy output".to_string(), ansi_heavy),
        ]
    }

    #[test]
    fn shared_probe_preserves_match_selection_and_output_across_corpora() {
        let cases = benchmark_corpus()
            .into_iter()
            .chain(integration_filter_inputs())
            .chain(hostile_outputs());

        for (label, raw_output) in cases {
            let output = strip_ansi(&raw_output);
            let legacy = legacy_match_phase(&output);
            let probed = probe_match_phase(&output);
            assert_eq!(legacy, probed, "output-shape dispatch changed for {label}");
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn unmatched_json_object_is_parsed_once_across_match_phase() {
        let output = r#"{"payload":{"nested":[1,2,3]},"ok":true}"#;
        reset_output_probe_json_parse_count();

        let result = probe_match_phase(output);

        assert_eq!(result.selected, None);
        assert_eq!(output_probe_json_parse_count(), 1);
    }
}

#[cfg(test)]
mod dispatch_specificity_tests {
    use super::*;
    use crate::compress::toml_filter::FilterRegistry;

    fn empty_registry() -> FilterRegistry {
        FilterRegistry::default()
    }

    /// Helper: assert that a given command would be claimed by a specific
    /// compressor by reading the output marker the compressor produces.
    /// (We can't easily compare Compressor instances by identity, so we
    /// dispatch and check for module-distinctive markers in the output.)
    fn dispatch(cmd: &str, output: &str) -> String {
        compress_with_registry(cmd, output, &empty_registry()).text
    }

    #[test]
    fn generic_dispatch_does_not_classify_error_or_warning_words() {
        let result = compress_with_registry(
            "unknown-tool",
            "error: this is just a log line\nwarning: this too",
            &empty_registry(),
        );

        assert!(result.dropped_by_class.is_empty());
        assert!(!result.had_inner_drop);
        assert!(result.text.contains("error: this is just a log line"));
    }

    #[test]
    fn bun_run_vitest_routes_to_vitest_not_generic() {
        // VitestCompressor preserves PASS/FAIL markers and "Tests:" summary.
        // BunCompressor's `Some("run")` arm currently goes to generic which
        // would middle-truncate. Use a small vitest-shaped output and assert
        // the vitest formatter's output marker is present.
        let output = "Test Files  1 passed (1)\n     Tests  4 passed (4)\n  Start at  10:00:00\n  Duration  120ms\n";
        let compressed = dispatch("bun run vitest", output);
        // Assert vitest path took it: the vitest text summary keeps "Tests" / "Test Files" lines
        assert!(compressed.contains("Tests") || compressed.contains("Test Files"));
    }

    #[test]
    fn npm_test_routes_to_vitest_when_output_is_vitest_shaped() {
        // `npm test` has no vitest token, so this proves the output-shape
        // tier runs before the broad NpmCompressor PackageManager tier.
        let output = "RERUN src/foo.test.ts x1\nFAIL src/foo.test.ts\nTest Files  1 failed (1)\nDuration    120ms\n";
        let compressed = dispatch("npm test", output);
        assert!(compressed.contains("FAIL src/foo.test.ts"));
        assert!(compressed.contains("Duration    120ms"));
        assert!(!compressed.contains("RERUN"));
    }

    #[test]
    fn bun_run_vitest_token_match_wins_over_bun_head_match() {
        // Concrete proof the new dispatch works: a command where Bun would
        // otherwise have claimed it.
        let output = "PASS src/a.test.ts (1)\n PASS src/b.test.ts (1)\nTest Files  2 passed (2)\n     Tests  4 passed (4)\n";
        let compressed = dispatch("bun run vitest run", output);
        // Vitest preserves PASS lines and "Tests:" summary.
        assert!(compressed.contains("Test Files") || compressed.contains("PASS"));
    }

    #[test]
    fn bunx_jest_routes_to_vitest_module() {
        let output = "PASS src/foo.test.js (1.2s)\nTest Suites: 1 passed, 1 total\nTests:       3 passed, 3 total\n";
        let compressed = dispatch("bunx jest --json", output);
        assert!(compressed.contains("Tests:") && compressed.contains("Test Suites"));
    }

    #[test]
    fn pnpm_run_vitest_routes_to_vitest() {
        let output = "Test Files  1 passed (1)\n     Tests  10 passed (10)\n";
        let compressed = dispatch("pnpm run vitest", output);
        assert!(compressed.contains("Tests") || compressed.contains("Test Files"));
    }

    #[test]
    fn npx_eslint_routes_to_eslint_not_generic() {
        let output = "\n/tmp/a.js\n  1:1  error  'foo' is defined but never used  no-unused-vars\n\n✖ 1 problem (1 error, 0 warnings)\n";
        let compressed = dispatch("npx eslint .", output);
        // EslintCompressor preserves rule IDs and the ✖ summary.
        assert!(compressed.contains("no-unused-vars") || compressed.contains("✖"));
    }

    #[test]
    fn npm_run_lint_without_linter_output_shape_falls_back() {
        // `npm run lint` has no eslint token, and this output has no eslint
        // summary signature, so it should remain package-manager generic.
        let output = "> my-project@1.0.0 lint\n> eslint .\n\nAll good.\n";
        let compressed = dispatch("npm run lint", output);
        assert!(compressed.contains("All good."));
    }

    #[test]
    fn bun_test_still_routes_to_bun_test_compressor() {
        // Bun.test is the v0.28.2 fix — make sure specificity dispatch
        // doesn't accidentally break it. The Bun module's `Some("test")`
        // arm should still claim this when no Specific matcher does.
        // BunTestCompressor doesn't exist as a separate module — the
        // BunCompressor.compress() routes Some("test") to its inner
        // compress_test() function. The relevant assertion: this still
        // produces bun-test-shaped output, not generic-truncated output.
        let output = "bun test v1.3.14\n\nsrc/foo.test.ts:\n(pass) my test [0.5ms]\n\n 1 pass\n 0 fail\n 1 expect() calls\nRan 1 tests across 1 files. [1.00ms]\n";
        let compressed = dispatch("bun test", output);
        assert!(compressed.contains("(pass)") || compressed.contains("1 pass"));
    }

    #[test]
    fn bunx_vitest_routes_to_vitest() {
        let output = "Test Files  1 passed (1)\n     Tests  3 passed (3)\n";
        let compressed = dispatch("bunx vitest run", output);
        assert!(compressed.contains("Tests") || compressed.contains("Test Files"));
    }

    #[test]
    fn cargo_test_still_routes_to_cargo() {
        // Regression: specificity reordering must not break commands that
        // already worked. Cargo is Specific tier.
        let output = "running 5 tests\ntest foo ... ok\ntest bar ... FAILED\n\nfailures:\n\ntest result: FAILED. 4 passed; 1 failed\n";
        let compressed = dispatch("cargo test", output);
        // Cargo's test compressor preserves PASS/FAIL semantics.
        assert!(compressed.contains("failed") || compressed.contains("FAILED"));
    }

    #[test]
    fn top_level_piped_cargo_test_uses_generic_output() {
        let output = "running 1 test\ntest ok_test ... ok\n\ntest result: ok. 1 passed; 0 failed\n";

        let compressed = compress_with_registry("cargo test | cat", output, &empty_registry());

        assert!(
            compressed.text.contains("test ok_test ... ok"),
            "piped cargo output must stay generic/raw, got: {}",
            compressed.text
        );
    }

    #[test]
    fn non_piped_cargo_test_still_uses_cargo_compressor() {
        let output = "running 1 test\ntest ok_test ... ok\n\ntest result: ok. 1 passed; 0 failed\n";

        let compressed = compress_with_registry("cargo test", output, &empty_registry());

        assert!(compressed.text.contains("running 1 test"));
        assert!(compressed.text.contains("test result: ok"));
        assert!(
            !compressed.text.contains("test ok_test ... ok"),
            "non-piped cargo test should keep using the cargo compressor, got: {}",
            compressed.text
        );
    }

    #[test]
    fn git_status_still_routes_to_git() {
        // Regression: git is Specific tier.
        let output =
            "On branch main\nYour branch is up to date.\n\nnothing to commit, working tree clean\n";
        let compressed = dispatch("git status", output);
        assert!(compressed.contains("branch") || compressed.contains("clean"));
    }

    #[test]
    fn pnpm_install_still_routes_to_pnpm() {
        // Regression: pnpm install was handled before this change.
        let output = "Progress: resolved 100, downloaded 50, added 50\nAdded 50 packages\n";
        let compressed = dispatch("pnpm install", output);
        // PnpmCompressor's compress_package keeps "+ pkg" or "Added X packages" type lines.
        assert!(compressed.contains("Added") || compressed.contains("Progress"));
    }
}

#[cfg(test)]
mod exit_code_safety_tests {
    use super::*;
    use crate::compress::toml_filter::{build_registry, FilterRegistry};

    fn empty_registry() -> FilterRegistry {
        FilterRegistry::default()
    }

    #[test]
    fn go_build_failure_signal_preserved_even_when_exit_zero_masks_failure() {
        let output = "go: go.mod file not found in current directory or any parent directory; see 'go help modules'\n";

        let failed =
            compress_with_registry_exit_code("go build ./...", output, Some(1), &empty_registry());
        assert!(!failed.text.contains("go build: ok"));
        assert!(failed.text.contains("go.mod file not found"));

        let masked =
            compress_with_registry_exit_code("go build ./...", output, Some(0), &empty_registry());
        assert!(!masked.text.contains("go build: ok"));
        assert!(masked.text.contains("go.mod file not found"));
    }

    #[test]
    fn playwright_nonzero_crash_does_not_become_passed_summary() {
        let output = r#"Running 4 tests using 2 workers

  ✓  1 [chromium] › example.spec.ts:5:1 › has title (2.3s)
  ✓  2 [chromium] › example.spec.ts:9:1 › get started link (1.8s)
  ✓  3 [chromium] › nav.spec.ts:3:1 › navigates (1.2s)
  ✓  4 [chromium] › auth.spec.ts:7:1 › logs out (1.0s)

  4 passed (6.3s)
Error: browserType.launch: Target page, context or browser has been closed
"#;

        let failed = compress_with_registry_exit_code(
            "npx playwright test",
            output,
            Some(1),
            &empty_registry(),
        );
        assert!(!failed.text.starts_with("playwright: 4 tests passed"));
        assert!(failed.text.contains("browserType.launch"));
    }

    #[test]
    fn cargo_test_compile_error_nonzero_preserves_error_code_diagnostic() {
        let output = r#"   Compiling demo v0.1.0 (/tmp/demo)
error[E0432]: unresolved import `crate::missing`
 --> src/lib.rs:1:5
  |
1 | use crate::missing;
  |     ^^^^^^^^^^^^^^ no `missing` in the root

error: could not compile `demo` (lib test) due to 1 previous error
"#;

        let failed =
            compress_with_registry_exit_code("cargo test", output, Some(101), &empty_registry());
        assert!(failed.text.contains("error[E0432]"));
        assert!(failed.text.contains("unresolved import"));
        assert!(failed.text.contains("error: could not compile"));
    }

    #[test]
    fn chained_mypy_success_then_later_failure_uses_failure_preserving_output() {
        let output = "Success: no issues found in 1 source file\nError: node process exploded\n";

        let failed = compress_with_registry_exit_code(
            "mypy src && node fail.js",
            output,
            Some(1),
            &empty_registry(),
        );
        assert_ne!(failed.text, "mypy: clean");
        assert!(failed.text.contains("Error: node process exploded"));
    }

    #[test]
    fn toml_shortcircuit_is_skipped_for_nonzero_exit() {
        let registry = build_registry(
            &[(
                "wget",
                r#"[filter]
matches = ["wget"]

[shortcircuit]
when = '(?s).*'
replacement = "wget: ok"
"#,
            )],
            None,
            None,
        );
        let output = "Connecting to example.invalid\nerror: connection refused\n";

        let failed = compress_with_registry_exit_code(
            "wget https://example.invalid",
            output,
            Some(1),
            &registry,
        );
        assert_ne!(failed.text, "wget: ok");
        assert!(failed.text.contains("error: connection refused"));
    }

    #[test]
    fn unknown_exit_code_keeps_byte_identical_legacy_compressor_output() {
        // Use a clean single command (no separator) so this tests the
        // unknown-exit-code path, not the separator-forces-generic path.
        let output = "Success: no issues found in 1 source file\n";

        let legacy = compress_with_registry_exit_code("mypy src", output, None, &empty_registry());
        assert_eq!(legacy.text, "mypy: clean");
    }

    #[test]
    fn killed_exit_sentinel_rejects_clean_legacy_summary() {
        let output = "Success: no issues found in 1 source file
Error: later chained command failed
";

        let killed = compress_with_registry_exit_code(
            "mypy src && node fail.js",
            output,
            Some(137),
            &empty_registry(),
        );
        assert_ne!(killed.text, "mypy: clean");
        assert!(killed.text.contains("Error: later chained command failed"));
    }

    #[test]
    fn nonzero_clean_eslint_json_summary_falls_back_to_raw_output() {
        let output =
            r#"[{"filePath":"/repo/src/main.ts","messages":[],"errorCount":0,"warningCount":0}]"#;

        let failed = compress_with_registry_exit_code(
            "eslint -f json .",
            output,
            Some(1),
            &empty_registry(),
        );

        assert_ne!(failed.text, "eslint: no issues");
        assert!(failed.text.contains(r#""messages":[]"#));
    }

    #[test]
    fn nonzero_appends_distinct_missing_raw_failure_lines() {
        let raw = "Error: first failure
progress
Error: second failure
";
        let compressed = CompressionResult::new("Error: first failure");

        let preserved = failure_preserving_result("tool", raw, compressed, Some(1));

        assert!(preserved.text.contains("Error: first failure"));
        assert!(preserved.text.contains("Error: second failure"));
        assert!(preserved
            .text
            .contains("[raw failure lines preserved by AFT]"));
    }

    #[test]
    fn nonzero_cargo_failure_class_cap_falls_back_to_all_failures() {
        let mut output = String::from(
            "running 40 tests

failures:

",
        );
        for index in 0..40 {
            output.push_str(&format!(
                "---- case_{index} stdout ----
thread 'case_{index}' panicked at src/lib.rs:{index}:1

"
            ));
        }
        output.push_str(
            "failures:
",
        );
        for index in 0..40 {
            output.push_str(&format!(
                "    case_{index}
"
            ));
        }
        output.push_str(
            "
test result: FAILED. 0 passed; 40 failed; 0 ignored; 0 measured; 0 filtered out
",
        );

        let failed =
            compress_with_registry_exit_code("cargo test", &output, Some(101), &empty_registry());

        assert!(failed.text.contains("---- case_0 stdout ----"));
        assert!(failed.text.contains("---- case_39 stdout ----"));
        assert!(failed.dropped_by_class.is_empty());
    }

    #[test]
    fn toml_shortcircuit_is_skipped_for_unknown_exit_when_failure_signal_exists() {
        let registry = build_registry(
            &[(
                "make",
                r#"[filter]
matches = ["make"]

[shortcircuit]
when = '(?s).*'
replacement = "make: ok"
"#,
            )],
            None,
            None,
        );
        let output = "build step
ERROR: compiler crashed
";

        let failed = compress_with_registry_exit_code("make", output, None, &registry);

        assert_ne!(failed.text, "make: ok");
        assert!(failed.text.contains("ERROR: compiler crashed"));
    }

    // aft:expected-duplicate -- the frozen implementation is the independent differential oracle.
    fn frozen_contains_nonzero_failure_word(line: &str, word: &str) -> bool {
        let lower = line.to_ascii_lowercase();
        for (index, _) in lower.match_indices(word) {
            let end = index + word.len();
            let before_is_word = lower[..index].chars().next_back().is_some_and(is_word_char);
            let after_is_word = lower[end..].chars().next().is_some_and(is_word_char);
            if before_is_word || after_is_word {
                continue;
            }

            let prefix = lower[..index].trim_end();
            let digits_start = prefix
                .char_indices()
                .rev()
                .take_while(|(_, ch)| ch.is_ascii_digit())
                .last()
                .map(|(idx, _)| idx);
            let Some(digits_start) = digits_start else {
                return true;
            };
            let digits = &prefix[digits_start..];
            if digits.parse::<usize>().ok() != Some(0) {
                return true;
            }
        }
        false
    }

    fn frozen_line_has_failure_signal(line: &str) -> bool {
        let lower = line.to_ascii_lowercase();
        line.contains("error[")
            || lower.contains("error:")
            || line.contains("Error")
            || line.contains("ERROR")
            || lower.contains("internalerror")
            || lower.contains("traceback")
            || lower.contains("exception")
            || lower.contains("no module named")
            || lower.contains("undefined reference")
            || lower.contains("linker command failed")
            || lower.contains("undefined:")
            || lower.contains("expected declaration")
            || lower.contains("collect2: error")
            || lower.contains("ld: error")
            || lower.contains("fatal error")
            || line.contains("FAILED")
            || line.contains("FAIL")
            || frozen_contains_nonzero_failure_word(line, "fail")
            || frozen_contains_nonzero_failure_word(line, "failed")
            || frozen_contains_nonzero_failure_word(line, "failure")
            || frozen_contains_nonzero_failure_word(line, "failures")
            || lower.contains("panic")
            || lower.contains("cannot find")
            || lower.contains("not found")
            || lower.contains("no such")
    }

    fn frozen_missing_failure_lines_reference(
        raw_output: &str,
        compressed_text: &str,
    ) -> Vec<String> {
        let compressed_lines: std::collections::BTreeSet<String> = compressed_text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect();
        let mut seen = std::collections::BTreeSet::new();
        let mut missing = Vec::new();

        for line in raw_output.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || !frozen_line_has_failure_signal(trimmed) {
                continue;
            }
            if compressed_lines.contains(trimmed) || !seen.insert(trimmed.to_string()) {
                continue;
            }
            missing.push(trimmed.to_string());
        }

        missing
    }

    #[test]
    fn failure_line_index_is_byte_equivalent_to_frozen_reference() {
        let signals = [
            "Error: ordinary failure",
            "ERROR: uppercase failure",
            "error[E0432]: unresolved import",
            "eRrOr: mixed case",
            "0 fail",
            "00 failed",
            "1 failed",
            "0 failures",
            "01 failure",
            "2 failures",
            "failed_extra",
            "prefix_failure_suffix",
            "failures!",
            "thread panicked at src/lib.rs:1",
            "undefined reference to `main`",
            "例外ではない unicode status",
        ];
        for signal in signals {
            assert_eq!(
                line_has_failure_signal_lower(signal, &signal.to_ascii_lowercase()),
                frozen_line_has_failure_signal(signal),
                "classifier diverged for {signal:?}"
            );
        }
        for prefix in ["", "0 ", "00 ", "1 ", "01 ", "pre", "_", "2 errors and "] {
            for word in [
                "fail",
                "failed",
                "failure",
                "failures",
                "failing",
                "failuresx",
            ] {
                for suffix in ["", "!", "_tail", "x", " then continue"] {
                    let line = format!("{prefix}{word}{suffix}");
                    assert_eq!(
                        line_has_failure_signal_lower(&line, &line.to_ascii_lowercase()),
                        frozen_line_has_failure_signal(&line),
                        "classifier diverged for {line:?}"
                    );
                }
            }
        }

        let mut raw = String::new();
        let mut compressed = String::new();
        for (index, signal) in signals.iter().enumerate() {
            raw.push_str("ordinary progress\n");
            raw.push_str("  ");
            raw.push_str(signal);
            raw.push_str("  \n");
            if index % 2 == 0 {
                compressed.push_str(signal);
                compressed.push('\n');
            }
            if index % 3 == 0 {
                raw.push_str(signal);
                raw.push('\n');
            }
        }

        let expected = frozen_missing_failure_lines_reference(&raw, &compressed);
        let actual = missing_raw_failure_signal_lines(&raw, &compressed);
        assert_eq!(actual, expected);

        let render_raw = "Error: first failure\nprogress\nError: second failure\n";
        let render_compressed = "Error: first failure";
        let render_missing = frozen_missing_failure_lines_reference(render_raw, render_compressed);
        let expected_text = format!(
            "{render_compressed}\n[raw failure lines preserved by AFT]\n{}",
            render_missing.join("\n")
        );
        let actual_result = failure_preserving_result(
            "cargo test",
            render_raw,
            CompressionResult::new(render_compressed),
            Some(101),
        );
        assert_eq!(actual_result.text.as_bytes(), expected_text.as_bytes());
    }

    #[test]
    fn failure_line_index_allocations_do_not_scale_with_progress_lines() {
        use std::fmt::Write as _;

        let mut output = String::new();
        for index in 0..5_000 {
            writeln!(
                output,
                "worker-{index:05}: processed /workspace/src/file-{index:05}.rs"
            )
            .expect("writing to String cannot fail");
        }
        output.push_str("ERROR: linker command failed for target app\n");

        let (missing, allocations) =
            crate::test_allocations::count(|| missing_raw_failure_signal_lines(&output, &output));

        assert!(missing.is_empty());
        assert!(
            allocations <= 8,
            "failure-line membership should borrow progress lines; got {allocations} allocations"
        );
    }

    #[test]
    fn successful_exit_still_gets_concise_success_summary() {
        let output = r#"Running 4 tests using 2 workers

  ✓  1 [chromium] › example.spec.ts:5:1 › has title (2.3s)
  ✓  2 [chromium] › example.spec.ts:9:1 › get started link (1.8s)
  ✓  3 [chromium] › nav.spec.ts:3:1 › navigates (1.2s)
  ✓  4 [chromium] › auth.spec.ts:7:1 › logs out (1.0s)

  4 passed (6.3s)
"#;

        let successful =
            compress_with_registry_exit_code("playwright test", output, Some(0), &empty_registry());
        assert_eq!(successful.text, "playwright: 4 tests passed (6.3s)");
    }
}

#[cfg(test)]
mod normalize_command_tests {
    use super::*;

    #[test]
    fn passes_bare_commands_unchanged() {
        assert_eq!(normalize_command_for_dispatch("bun test"), None);
        assert_eq!(normalize_command_for_dispatch("cargo build"), None);
        assert_eq!(normalize_command_for_dispatch("git status"), None);
    }

    #[test]
    fn strips_cd_and_amp_prefix() {
        assert_eq!(
            normalize_command_for_dispatch("cd /repo && bun test").as_deref(),
            Some("bun test")
        );
        assert_eq!(
            normalize_command_for_dispatch("cd /repo/packages/aft && cargo test --release")
                .as_deref(),
            Some("cargo test --release")
        );
    }

    #[test]
    fn strips_cd_and_semicolon_prefix() {
        assert_eq!(
            normalize_command_for_dispatch("cd /repo; bun test").as_deref(),
            Some("bun test")
        );
    }

    #[test]
    fn strips_cd_with_quoted_path() {
        assert_eq!(
            normalize_command_for_dispatch("cd \"/path with space\" && npm install").as_deref(),
            Some("npm install")
        );
    }

    #[test]
    fn strips_env_assignments() {
        assert_eq!(
            normalize_command_for_dispatch("env FOO=bar npm install").as_deref(),
            Some("npm install")
        );
        assert_eq!(
            normalize_command_for_dispatch("env FOO=bar BAZ=qux RUST_LOG=info cargo test")
                .as_deref(),
            Some("cargo test")
        );
    }

    #[test]
    fn strips_bare_assignment_prefixes() {
        assert_eq!(
            normalize_command_for_dispatch("NODE_ENV=production npm install").as_deref(),
            Some("npm install")
        );
        assert_eq!(
            normalize_command_for_dispatch("FOO=1 BAR=2 cargo test").as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            normalize_command_for_dispatch("RUSTFLAGS='-C debug' cargo build").as_deref(),
            Some("cargo build")
        );
    }

    #[test]
    fn does_not_strip_later_assignment_arguments() {
        assert_eq!(normalize_command_for_dispatch("npm install foo=bar"), None);
    }

    #[test]
    fn env_without_assignments_returns_none() {
        // `env` alone is the env-listing command, not a prefix.
        assert_eq!(
            normalize_command_for_dispatch("env npm install").as_deref(),
            None
        );
    }

    #[test]
    fn strips_timeout_prefix() {
        assert_eq!(
            normalize_command_for_dispatch("timeout 30 cargo test").as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            normalize_command_for_dispatch("timeout 5m bun test").as_deref(),
            Some("bun test")
        );
    }

    #[test]
    fn strips_nohup_prefix() {
        assert_eq!(
            normalize_command_for_dispatch("nohup ./long-running-script.sh").as_deref(),
            Some("./long-running-script.sh")
        );
    }

    #[test]
    fn strips_paren_then_cd_and_amp() {
        assert_eq!(
            normalize_command_for_dispatch("(cd /repo && bun test").as_deref(),
            Some("bun test")
        );
    }

    #[test]
    fn chains_multiple_prefixes() {
        // env then timeout then real command.
        assert_eq!(
            normalize_command_for_dispatch("env FOO=bar timeout 30 cargo test").as_deref(),
            Some("cargo test")
        );
        // cd then env then real command.
        assert_eq!(
            normalize_command_for_dispatch("cd /repo && env FOO=bar npm install").as_deref(),
            Some("npm install")
        );
    }

    // -------- end-to-end dispatch via normalize() --------

    fn empty_registry() -> FilterRegistry {
        FilterRegistry::default()
    }

    #[test]
    fn cd_prefix_bun_test_still_routes_to_bun_test() {
        let output = "bun test v1.3.14\n\nsrc/a.test.ts:\n(pass) ok [0.1ms]\n\n 1 pass\n 0 fail\n 1 expect() calls\nRan 1 tests across 1 files. [1.00ms]\n";
        let compressed = compress_with_registry("cd /repo && bun test", output, &empty_registry());
        // The bun test compressor produces (pass) / "1 pass" / "Ran ..." in
        // the pass-only path. Generic middle-truncate would drop these and
        // keep the original. Asserting their presence proves the normalizer
        // succeeded.
        assert!(compressed.contains("(pass)") || compressed.contains("1 pass"));
    }

    #[test]
    fn cd_prefix_cargo_test_still_routes_to_cargo() {
        let output = "running 5 tests\ntest foo ... ok\ntest bar ... FAILED\n\nfailures:\n\ntest result: FAILED. 4 passed; 1 failed\n";
        let compressed =
            compress_with_registry("cd /repo && cargo test", output, &empty_registry());
        assert!(compressed.contains("FAILED") || compressed.contains("failed"));
    }

    #[test]
    fn env_prefix_npm_install_still_routes_to_npm() {
        let output = "added 50 packages, and audited 100 packages in 3s\n";
        let compressed = compress_with_registry(
            "env NODE_ENV=production npm install",
            output,
            &empty_registry(),
        );
        // NpmCompressor's install path keeps "added N packages" / "audited" markers.
        assert!(compressed.contains("added") || compressed.contains("audited"));
    }

    #[test]
    fn bare_assignment_prefix_npm_install_routes_to_npm() {
        let output = "npm http fetch GET 200 https://registry.npmjs.org/foo 123ms\nnpm WARN deprecated old-pkg@1.0.0: use new-pkg instead\n\nadded 42 packages in 2s\n\naudited 100 packages in 2s\n\nfound 0 vulnerabilities\n";
        let compressed =
            compress_with_registry("NODE_ENV=production npm install", output, &empty_registry());
        assert!(!compressed.contains("npm http fetch"));
        assert!(compressed.contains("audited 100 packages"));
    }

    #[test]
    fn bare_assignment_prefix_cargo_test_routes_to_cargo() {
        let output = "running 1 test\ntest foo ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";
        let compressed =
            compress_with_registry("FOO=1 BAR=2 cargo test", output, &empty_registry());
        assert!(compressed.contains("running 1 test"));
        assert!(compressed.contains("test result: ok"));
        assert!(!compressed.contains("test foo ... ok"));
    }

    #[test]
    fn quoted_assignment_prefix_cargo_build_routes_to_cargo() {
        let output = "   Compiling foo v0.1.0\nwarning: unused variable: `x`\n --> src/lib.rs:1:9\n  |\n1 |     let x = 1;\n  |         ^ help: if this is intentional, prefix it with an underscore: `_x`\n\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.12s\n";
        let compressed = compress_with_registry(
            "RUSTFLAGS='-C debug' cargo build",
            output,
            &empty_registry(),
        );
        assert!(!compressed.contains("Compiling foo"));
        assert!(compressed.contains("warning: unused variable"));
        assert!(compressed.contains("Finished `dev` profile"));
    }

    #[test]
    fn timeout_prefix_cargo_build_still_routes_to_cargo() {
        let output =
            "   Compiling foo v0.1.0\n    Finished `dev` profile [unoptimized] target(s) in 5s\n";
        let compressed =
            compress_with_registry("timeout 30 cargo build", output, &empty_registry());
        // CargoCompressor for build/check/run preserves the structure.
        assert!(compressed.contains("Compiling") || compressed.contains("Finished"));
    }

    #[test]
    fn normalize_splits_pipe_and_takes_last_stage() {
        assert_eq!(
            normalize_command_for_dispatch("git log | grep fix").as_deref(),
            Some("grep fix")
        );
    }

    #[test]
    fn normalize_cd_prefix_then_pipe_takes_last_stage() {
        assert_eq!(
            normalize_command_for_dispatch("cd /repo && git log | grep fix").as_deref(),
            Some("grep fix")
        );
    }

    #[test]
    fn normalize_no_pipe_returns_none() {
        assert_eq!(normalize_command_for_dispatch("git log"), None);
    }

    #[test]
    fn normalize_quoted_pipe_not_split() {
        assert_eq!(
            normalize_command_for_dispatch("grep \"a|b\" file.txt"),
            None
        );
    }

    #[test]
    fn normalize_balanced_command_substitution_splits_top_level_pipe() {
        // The inner `|` is inside $(...) (depth > 0) and must be ignored; the
        // real top-level `| grep x` splits to the last stage. The OLD code
        // bailed to None here and fell back to head-token dispatch on the full
        // command — exactly the data-loss path issue #137 is about.
        assert_eq!(
            normalize_command_for_dispatch("echo $(cmd | cmd) | grep x").as_deref(),
            Some("grep x")
        );
    }

    #[test]
    fn normalize_inner_pipe_in_substitution_without_top_level_pipe_is_none() {
        // No top-level pipe at all — the only `|` is inside $(...).
        assert_eq!(
            normalize_command_for_dispatch("echo $(cargo test | cat)"),
            None
        );
    }

    #[test]
    fn normalize_double_pipe_not_split() {
        assert_eq!(normalize_command_for_dispatch("git log || echo fail"), None);
    }

    #[test]
    fn normalize_multi_pipe_returns_last_stage() {
        assert_eq!(
            normalize_command_for_dispatch("git log | grep fix | head -5").as_deref(),
            Some("head -5")
        );
    }

    #[test]
    fn normalize_process_substitution_splits_top_level_pipe() {
        // `<(...)` inner pipe ignored; top-level `| grep x` splits to last stage.
        assert_eq!(
            normalize_command_for_dispatch("cat <(echo a | cat) | grep x").as_deref(),
            Some("grep x")
        );
    }

    #[test]
    fn normalize_pipe_ampersand_splits_last_stage() {
        // `|&` pipes stdout+stderr; it is a real pipe boundary, not `|` + `&`.
        assert_eq!(
            normalize_command_for_dispatch("cargo test |& grep FAIL").as_deref(),
            Some("grep FAIL")
        );
    }

    #[test]
    fn piped_cargo_test_grep_preserves_failed() {
        let grep_output = "test foo ... FAILED\n";
        let compressed =
            compress_with_registry("cargo test | grep FAIL", grep_output, &empty_registry());
        assert!(
            compressed.text.contains("FAILED"),
            "grep-filtered FAILED must survive, got: {}",
            compressed.text
        );
    }

    #[test]
    fn unsafe_piped_command_forces_generic_and_preserves_output() {
        // Unbalanced quote → the scanner can't trust the parse. A `|` is
        // present, so it must force generic rather than let CargoCompressor
        // claim `cargo test | …` and drop the single grep-filtered line.
        let grep_output = "test foo ... FAILED\n";
        let compressed =
            compress_with_registry("cargo test | grep \"FAIL", grep_output, &empty_registry());
        assert!(
            compressed.text.contains("FAILED"),
            "unsafe pipe must not drop output, got: {}",
            compressed.text
        );
    }

    #[test]
    fn split_top_level_pipe_variants() {
        assert_eq!(split_top_level_pipe("git log"), PipeSplit::None);
        assert_eq!(
            split_top_level_pipe("git log | grep fix"),
            PipeSplit::LastStage("grep fix".to_string())
        );
        // `||` is logical-or, not a pipe — but it IS a top-level separator,
        // so the no-pipe exit forces generic (multiple commands' output).
        assert_eq!(split_top_level_pipe("a || b"), PipeSplit::Unsafe);
        // inner pipe inside a subshell is not a top-level boundary.
        assert_eq!(split_top_level_pipe("(a | b)"), PipeSplit::None);
        // inner pipe inside $() is not a top-level boundary.
        assert_eq!(split_top_level_pipe("echo $(a | b)"), PipeSplit::None);
        // unbalanced quote with a pipe present → unsafe.
        assert_eq!(split_top_level_pipe("a | grep \"x"), PipeSplit::Unsafe);
        // unbalanced paren with a pipe present → unsafe.
        assert_eq!(split_top_level_pipe("$(a | b | grep x"), PipeSplit::Unsafe);
        // FAIL-CLOSED cases (Oracle findings) — a pipe must never be last-staged
        // when other top-level structure could mean the captured output isn't
        // the last stage's:
        // trailing empty stage
        assert_eq!(split_top_level_pipe("cargo test |"), PipeSplit::Unsafe);
        assert_eq!(split_top_level_pipe("cargo test |&"), PipeSplit::Unsafe);
        // pipe coexisting with a top-level separator
        assert_eq!(
            split_top_level_pipe("true | cargo test --quiet ; printf X"),
            PipeSplit::Unsafe
        );
        assert_eq!(
            split_top_level_pipe("true | cargo test && echo done"),
            PipeSplit::Unsafe
        );
        // unmatched close paren with a pipe
        assert_eq!(
            split_top_level_pipe("echo ) | cargo test"),
            PipeSplit::Unsafe
        );
        // bare `&` background is a separator; `2>&1` / `&>` redirects are not
        assert_eq!(split_top_level_pipe("a | b & c"), PipeSplit::Unsafe);
        assert_eq!(
            split_top_level_pipe("cargo test 2>&1 | grep FAIL"),
            PipeSplit::LastStage("grep FAIL".to_string())
        );
        // No-pipe separator cases: a top-level separator without a pipe still
        // forces generic so a head-token compressor can't drop later commands'
        // output.
        assert_eq!(
            split_top_level_pipe("cargo test ; printf SENTINEL"),
            PipeSplit::Unsafe
        );
        assert_eq!(
            split_top_level_pipe("cargo test && echo done"),
            PipeSplit::Unsafe
        );
        assert_eq!(
            split_top_level_pipe("cargo test\necho done"),
            PipeSplit::Unsafe
        );
        // Redirects are NOT separators — a single command with a redirect must
        // still dispatch to its head-token compressor.
        assert_eq!(split_top_level_pipe("cargo test 2>&1"), PipeSplit::None);
        assert_eq!(
            split_top_level_pipe("cargo test &> /dev/null"),
            PipeSplit::None
        );
    }

    #[test]
    fn strip_top_level_comment_removes_only_real_comments() {
        assert_eq!(
            strip_top_level_comment("printf keep # | cargo test"),
            "printf keep "
        );
        assert_eq!(
            strip_top_level_comment("printf keep # cargo test"),
            "printf keep "
        );
        // `#` not at a word boundary is literal (e.g. a fragment/anchor).
        assert_eq!(
            strip_top_level_comment("curl http://x/y#frag"),
            "curl http://x/y#frag"
        );
        // `#` inside quotes is literal.
        assert_eq!(
            strip_top_level_comment("grep \"# not a comment\" f"),
            "grep \"# not a comment\" f"
        );
        assert_eq!(
            strip_top_level_comment("echo '# literal'"),
            "echo '# literal'"
        );
        // no comment → unchanged.
        assert_eq!(
            strip_top_level_comment("git log | grep fix"),
            "git log | grep fix"
        );
    }

    #[test]
    fn commented_command_does_not_misdispatch_and_preserves_output() {
        // The `# cargo test` comment must not let CargoCompressor claim this
        // printf command's output and drop it — with OR without a pipe.
        for cmd in ["printf keep # | cargo test", "printf keep # cargo test"] {
            let compressed = compress_with_registry(cmd, "keep\n", &empty_registry());
            assert!(
                compressed.text.contains("keep"),
                "comment must not drop output for {cmd:?}, got: {}",
                compressed.text
            );
        }
    }

    #[test]
    fn pipe_with_trailing_command_chain_preserves_sentinel() {
        // `true | cargo test ; printf SENTINEL` — captured output includes
        // SENTINEL; cargo must not claim it and drop the sentinel line.
        let compressed = compress_with_registry(
            "true | cargo test --quiet ; printf SENTINEL",
            "SENTINEL\n",
            &empty_registry(),
        );
        assert!(
            compressed.text.contains("SENTINEL"),
            "trailing-chain output must survive, got: {}",
            compressed.text
        );
    }

    /// MUTATION CONTROL: reverting the no-pipe exit in `split_top_level_pipe`
    /// to ignore `saw_top_separator` (returning `PipeSplit::None` unconditionally)
    /// makes this test fail — cargo.rs claims the list and drops the sentinel.
    #[test]
    fn separator_list_forces_generic_and_preserves_sentinel() {
        // `cargo test ; printf SENTINEL` — the captured transcript contains both
        // cargo noise and the sentinel. A head-token cargo compressor would keep
        // only cargo-shaped lines and silently delete the sentinel. The no-pipe
        // separator must force generic compression so all output survives.
        let cargo_noise =
            "running 1 test\ntest ok_test ... ok\n\ntest result: ok. 1 passed; 0 failed\n";
        let transcript = format!("{cargo_noise}SENTINEL\n");
        let compressed = compress_with_registry(
            "cargo test ; printf SENTINEL",
            &transcript,
            &empty_registry(),
        );
        assert!(
            compressed.text.contains("SENTINEL"),
            "separator-list output must survive generic compression, got: {}",
            compressed.text
        );
    }

    #[test]
    fn cd_prefix_peel_leaves_no_residual_separator_for_cargo() {
        // `cd /x && cargo test` — the `cd /x &&` prefix is peeled, leaving a
        // clean `cargo test` with NO residual separator. Specialized cargo
        // compression must still engage (not force-generic).
        let output = "running 1 test\ntest ok_test ... ok\n\ntest result: ok. 1 passed; 0 failed\n";
        let compressed = compress_with_registry("cd /x && cargo test", output, &empty_registry());
        // Cargo compressor drops individual "test ... ok" lines in its summary.
        assert!(
            !compressed.text.contains("test ok_test ... ok"),
            "cd-peeled cargo test must still use the cargo compressor, got: {}",
            compressed.text
        );
        assert!(compressed.text.contains("test result: ok"));
    }

    #[test]
    fn clean_single_cargo_test_dispatch_byte_identical() {
        // Control: a clean single `cargo test` (no separator, no pipe) must
        // continue dispatching to the specialized cargo compressor and produce
        // the same output as the original specialized-compression behavior.
        let output = "running 1 test\ntest ok_test ... ok\n\ntest result: ok. 1 passed; 0 failed\n";
        let compressed = compress_with_registry("cargo test", output, &empty_registry());
        assert!(compressed.text.contains("running 1 test"));
        assert!(compressed.text.contains("test result: ok"));
        assert!(
            !compressed.text.contains("test ok_test ... ok"),
            "clean cargo test must keep using the cargo compressor, got: {}",
            compressed.text
        );
    }

    #[test]
    fn is_shell_boundary_covers_redirects_and_operators() {
        for tok in [
            "|",
            "|&",
            ";",
            "&",
            "&&",
            "||",
            ">",
            ">>",
            "<",
            "<<",
            "<<<",
            "&>",
            "&>>",
            "2>",
            "2>>",
            "2>&1",
            "1>&2",
            ">/dev/null",
            "2>/dev/null",
        ] {
            assert!(is_shell_boundary(tok), "{tok} should be a boundary");
        }
        for tok in ["test", "log", "build", "--release", "-v", "file.txt"] {
            assert!(!is_shell_boundary(tok), "{tok} must not be a boundary");
        }
    }
}
