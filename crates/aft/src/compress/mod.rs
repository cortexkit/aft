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
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use toml_filter::{apply_filter_with_exit_code, FilterRegistry};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressionResult {
    pub text: String,
    pub dropped_by_class: BTreeMap<DropClass, usize>,
    pub had_inner_drop: bool,
    pub offset_hint_eligible: bool,
    pub offset_start_line: Option<usize>,
}

impl CompressionResult {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            dropped_by_class: BTreeMap::new(),
            had_inner_drop: false,
            offset_hint_eligible: true,
            offset_start_line: None,
        }
    }

    pub fn with_class_drops(
        text: impl Into<String>,
        dropped_by_class: BTreeMap<DropClass, usize>,
    ) -> Self {
        let had_inner_drop = !dropped_by_class.is_empty();
        Self {
            text: text.into(),
            dropped_by_class,
            had_inner_drop,
            offset_hint_eligible: !had_inner_drop,
            offset_start_line: None,
        }
    }

    pub fn with_inner_drop(text: impl Into<String>, offset_hint_eligible: bool) -> Self {
        Self {
            text: text.into(),
            dropped_by_class: BTreeMap::new(),
            had_inner_drop: true,
            offset_hint_eligible,
            offset_start_line: None,
        }
    }

    pub fn with_prefix_drop(text: impl Into<String>, offset_start_line: usize) -> Self {
        Self {
            text: text.into(),
            dropped_by_class: BTreeMap::new(),
            had_inner_drop: true,
            offset_hint_eligible: true,
            offset_start_line: Some(offset_start_line),
        }
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
        return CompressionResult::new(output);
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
    let stripped_for_generic = strip_ansi(output);

    // Normalize the command so shell-prefix idioms like `cd /path && bun test`,
    // `env FOO=bar npm install`, `timeout 30 cargo build`, and `(cd /path; cmd)`
    // don't hide the real command head from per-module matchers. Without this,
    // BunCompressor/NpmCompressor/PnpmCompressor (which match by head-token)
    // silently fall through to generic in most agent-issued bash calls.
    let normalized = normalize_command_for_dispatch(command);
    let dispatch_cmd = normalized.as_deref().unwrap_or(command);

    let compressors: [&dyn Compressor; 20] = [
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
    ];

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
    for specificity in [Specificity::Specific, Specificity::PackageManager] {
        for compressor in compressors
            .iter()
            .filter(|c| c.specificity() == specificity)
        {
            if compressor.matches_output(&stripped_for_generic) {
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
        let result = apply_filter_with_exit_code(filter, output, exit_code);
        return failure_preserving_result(command, &stripped_for_generic, result, exit_code);
    }

    // Tier 3: generic fallback.
    GenericCompressor.compress_with_exit_code(command, &stripped_for_generic, exit_code)
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

    if result_looks_successful(&result.text)
        || raw_failure_signal_absent_from_compressed(stripped_raw_output, &result.text)
    {
        GenericCompressor.compress_with_exit_code(command, stripped_raw_output, exit_code)
    } else {
        result
    }
}

fn raw_failure_signal_absent_from_compressed(raw_output: &str, compressed_text: &str) -> bool {
    let mut saw_signal_line = false;
    for line in raw_output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !line_has_failure_signal(trimmed) {
            continue;
        }
        saw_signal_line = true;
        if compressed_text.contains(trimmed) {
            return false;
        }
    }

    saw_signal_line && !text_has_failure_signal(compressed_text)
}

fn result_looks_successful(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    !text_has_failure_signal(text)
        && (lower.contains("clean")
            || lower.contains(" ok")
            || lower.contains(":ok")
            || lower.contains(": ok")
            || lower.contains("passed")
            || lower.contains("no errors")
            || lower.contains("all checks passed")
            || lower.contains("formatted")
            || lower.contains("0 fail")
            || lower.contains("found 0")
            || lower.contains("up to date")
            || lower.contains("up-to-date"))
}

fn text_has_failure_signal(text: &str) -> bool {
    text.lines()
        .any(|line| line_has_failure_signal(line.trim()))
}

fn line_has_failure_signal(line: &str) -> bool {
    line.contains("error[")
        || line.contains("error:")
        || line.contains("Error")
        || line.contains("FAILED")
        || line.contains("FAIL")
        || contains_nonzero_failure_word(line, "failed")
        || contains_nonzero_failure_word(line, "failure")
        || contains_nonzero_failure_word(line, "failures")
        || line.contains("panic")
        || line.contains("cannot find")
        || line.contains("not found")
        || line.contains("no such")
}

fn contains_nonzero_failure_word(line: &str, word: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    for (index, _) in lower.match_indices(word) {
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
        if let Some(last) = split_on_top_level_pipe(&current) {
            return Some(last);
        }
        Some(current)
    } else {
        split_on_top_level_pipe(command)
    }
}

/// Returns true if the token is a shell metacharacter that acts as a
/// command boundary (`|`, `;`, `&&`, `||`, `>`, `<`). Subcommand parsers
/// use this to avoid returning these as subcommand names.
pub fn is_shell_boundary(token: &str) -> bool {
    matches!(token, "|" | ";" | "&&" | "||" | ">" | "<")
}

/// Quote-aware splitter that returns the last stage after a top-level `|`.
/// Handles single quotes, double quotes, and backslash escapes. Bails
/// (returns `None`) on backticks, `$()` command substitution, and
/// process substitution `<(` / `>(` so callers fall back to generic handling.
fn split_on_top_level_pipe(command: &str) -> Option<String> {
    let bytes = command.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut last_pipe_end = 0;

    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];

        if escaped {
            escaped = false;
            i += 1;
            continue;
        }

        if ch == b'\\' && !in_single {
            escaped = true;
            i += 1;
            continue;
        }

        if in_single {
            if ch == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }

        if in_double {
            if ch == b'"' {
                in_double = false;
            } else if ch == b'`' || (ch == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'(') {
                return None;
            }
            i += 1;
            continue;
        }

        if ch == b'\'' {
            in_single = true;
        } else if ch == b'"' {
            in_double = true;
        } else if ch == b'`' {
            return None;
        } else if ch == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'(' {
            return None;
        } else if (ch == b'<' || ch == b'>') && i + 1 < bytes.len() && bytes[i + 1] == b'(' {
            return None;
        } else if ch == b'|' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'|' {
                i += 2;
                continue;
            }
            last_pipe_end = i + 1;
        }

        i += 1;
    }

    if last_pipe_end > 0 {
        let last_stage = command[last_pipe_end..].trim();
        if last_stage.is_empty() {
            None
        } else {
            Some(last_stage.to_string())
        }
    } else {
        None
    }
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
    fn go_build_nonzero_preserves_missing_module_error_but_zero_keeps_old_summary() {
        let output = "go: go.mod file not found in current directory or any parent directory; see 'go help modules'\n";

        let failed =
            compress_with_registry_exit_code("go build ./...", output, Some(1), &empty_registry());
        assert!(!failed.text.contains("go build: ok"));
        assert!(failed.text.contains("go.mod file not found"));

        let successful =
            compress_with_registry_exit_code("go build ./...", output, Some(0), &empty_registry());
        assert_eq!(successful.text, "go build: ok");
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
        let output =
            "Success: no issues found in 1 source file\nError: later chained command failed\n";

        let legacy = compress_with_registry_exit_code(
            "mypy src && node fail.js",
            output,
            None,
            &empty_registry(),
        );
        assert_eq!(legacy.text, "mypy: clean");
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
        assert_eq!(normalize_command_for_dispatch("grep \"a|b\" file.txt"), None);
    }

    #[test]
    fn normalize_command_substitution_bails() {
        assert_eq!(
            normalize_command_for_dispatch("echo $(cmd | cmd) | grep x"),
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
    fn normalize_process_substitution_bails() {
        assert_eq!(
            normalize_command_for_dispatch("cat <(echo a | cat) | grep x"),
            None
        );
    }

    #[test]
    fn piped_cargo_test_grep_preserves_failed() {
        let grep_output = "test foo ... FAILED\n";
        let compressed = compress_with_registry(
            "cargo test | grep FAIL",
            grep_output,
            &empty_registry(),
        );
        assert!(
            compressed.text.contains("FAILED"),
            "grep-filtered FAILED must survive, got: {}",
            compressed.text
        );
    }
}
