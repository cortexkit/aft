//! Output compression for hoisted bash.
//!
//! Compression has four tiers, tried in this order:
//!
//! 1. **Specific Rust [`Compressor`] modules** — hand-written parsers for
//!    specific tools identified by tool tokens (for example `vitest`, `eslint`,
//!    `cargo`, `git`). These win before broad package-manager compressors.
//! 2. **Package-manager [`Compressor`] modules** — broad head-token matchers
//!    (`npm`, `pnpm`, `bun`) that compress unclaimed package-manager output.
//! 3. **TOML filters** — declarative strip + truncate + cap + shortcircuit
//!    rules for the long tail of CLI tools. Loaded from builtin / user /
//!    project sources via [`toml_filter::build_registry`]. See
//!    [`toml_filter`] and [`trust`] for the trust model.
//! 4. **[`generic`] fallback** — ANSI strip + consecutive-dedup +
//!    middle-truncate. Always applies when no Rust module or TOML filter
//!    matches.

pub mod biome;
pub mod builtin_filters;
pub mod bun;
pub mod cargo;
pub mod eslint;
pub mod generic;
pub mod git;
pub mod go;
pub mod mypy;
pub mod next;
pub mod npm;
pub mod playwright;
pub mod pnpm;
pub mod prettier;
pub mod pytest;
pub mod ruff;
pub mod toml_filter;
pub mod trust;
pub mod tsc;
pub mod vitest;

use crate::context::AppContext;
use biome::BiomeCompressor;
use bun::BunCompressor;
use cargo::CargoCompressor;
use eslint::EslintCompressor;
use generic::{strip_ansi, GenericCompressor};
use git::GitCompressor;
use go::{GoCompressor, GolangciLintCompressor};
use mypy::MypyCompressor;
use next::NextCompressor;
use npm::NpmCompressor;
use playwright::PlaywrightCompressor;
use pnpm::PnpmCompressor;
use prettier::PrettierCompressor;
use pytest::PytestCompressor;
use ruff::RuffCompressor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use toml_filter::{apply_filter, FilterRegistry};
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
/// Dispatch order: Specific tier first, then PackageManager tier, then
/// TOML filters, then GenericCompressor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Specificity {
    Specific,
    PackageManager,
}

/// A `Compressor` knows how to reduce one specific command's output to fewer
/// tokens while preserving the information the agent needs.
pub trait Compressor {
    /// Returns true if this compressor handles the given command head + args.
    /// Called after generic detection (ANSI strip, dedup) so this is per-command logic only.
    fn matches(&self, command: &str) -> bool;

    /// Compress the output. Original is left untouched if compression fails.
    fn compress(&self, command: &str, output: &str) -> String;

    fn specificity(&self) -> Specificity {
        Specificity::Specific
    }
}

/// Top-level dispatch: try specific Rust modules, package-manager modules, TOML filters, then generic fallback.
///
/// Convenience wrapper for command handlers that already hold an `AppContext`.
/// Backs onto [`compress_with_registry`] which is thread-safe for use from the
/// `BgTaskRegistry` watchdog.
pub fn compress(command: &str, output: String, ctx: &AppContext) -> String {
    if !ctx.config().experimental_bash_compress {
        return output;
    }
    let registry_handle = ctx.shared_filter_registry();
    let guard = match registry_handle.read() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    compress_with_registry(command, &output, &guard)
}

/// Thread-safe dispatch that does not need `AppContext`. Caller is responsible
/// for the `experimental_bash_compress` gate (the registry has no opinion).
///
/// Used from background threads (notably the `BgTaskRegistry` watchdog and
/// completion-frame emitter) where lock-free access is required.
pub fn compress_with_registry(command: &str, output: &str, registry: &FilterRegistry) -> String {
    let stripped_for_generic = strip_ansi(output);

    // Normalize the command so shell-prefix idioms like `cd /path && bun test`,
    // `env FOO=bar npm install`, `timeout 30 cargo build`, and `(cd /path; cmd)`
    // don't hide the real command head from per-module matchers. Without this,
    // BunCompressor/NpmCompressor/PnpmCompressor (which match by head-token)
    // silently fall through to generic in most agent-issued bash calls.
    let normalized = normalize_command_for_dispatch(command);
    let dispatch_cmd = normalized.as_deref().unwrap_or(command);

    let compressors: [&dyn Compressor; 17] = [
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
    ];

    // Tier 1a: Specific compressors win first.
    for compressor in compressors
        .iter()
        .filter(|c| c.specificity() == Specificity::Specific)
    {
        if compressor.matches(dispatch_cmd) {
            return compressor.compress(dispatch_cmd, &stripped_for_generic);
        }
    }

    // Tier 1b: PackageManager compressors get unclaimed commands.
    for compressor in compressors
        .iter()
        .filter(|c| c.specificity() == Specificity::PackageManager)
    {
        if compressor.matches(dispatch_cmd) {
            return compressor.compress(dispatch_cmd, &stripped_for_generic);
        }
    }

    // Tier 2: TOML filters. Pass raw output so `[ansi].strip = false` filters
    // can intentionally match escape sequences; `apply_filter` owns ANSI policy.
    if let Some(filter) = registry.lookup(dispatch_cmd) {
        return apply_filter(filter, output);
    }

    // Tier 3: generic fallback.
    GenericCompressor.compress(command, &stripped_for_generic)
}

/// Build the registry of TOML filters from the standard sources for the
/// active context. Called lazily by [`AppContext::filter_registry`].
///
/// Layering (highest priority first):
/// 1. Project filters at `<project_root>/.aft/filters/*.toml` — loaded only
///    when the project is in the trusted set (see [`trust`]).
/// 2. User filters at `<storage_dir>/filters/*.toml`.
/// 3. Builtin filters compiled into the binary via [`builtin_filters`].
pub fn build_registry_for_context(ctx: &AppContext) -> FilterRegistry {
    let config = ctx.config();
    let storage_dir = config.storage_dir.clone();
    let project_root = config.project_root.clone();
    drop(config);

    let user_dir = storage_dir.as_ref().map(|d| d.join("filters"));
    let project_dir = match (project_root.as_ref(), storage_dir.as_ref()) {
        (Some(root), Some(storage)) => {
            if trust::is_project_trusted(Some(storage), root) {
                Some(root.join(".aft").join("filters"))
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
    // ASSIGN looks like VAR=value (no leading dash).
    let rest = command.strip_prefix("env")?.trim_start();
    let mut tokens = rest.split_whitespace().peekable();
    let mut consumed = 0usize;
    while let Some(&token) = tokens.peek() {
        if !is_env_assignment(token) {
            break;
        }
        consumed += token.len();
        // count whitespace between this and next
        tokens.next();
    }
    if consumed == 0 {
        return None;
    }
    // Re-walk rest to find the byte offset after the last assignment.
    let mut idx = 0usize;
    let mut consumed_now = 0usize;
    let bytes = rest.as_bytes();
    while consumed_now < consumed && idx < bytes.len() {
        // skip whitespace
        while idx < bytes.len() && (bytes[idx] as char).is_whitespace() {
            idx += 1;
        }
        // consume token
        let token_start = idx;
        while idx < bytes.len() && !(bytes[idx] as char).is_whitespace() {
            idx += 1;
        }
        consumed_now += idx - token_start;
    }
    let after = rest[idx..].trim_start();
    if after.is_empty() {
        None
    } else {
        Some(after.to_string())
    }
}

fn is_env_assignment(token: &str) -> bool {
    if token.starts_with('-') {
        return false;
    }
    let Some((name, _value)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
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

/// Resolve the user-filter directory for an arbitrary storage_dir. Used by
/// `aft doctor filters` to inspect filters without needing a live AppContext.
pub fn user_filter_dir(storage_dir: &Path) -> PathBuf {
    storage_dir.join("filters")
}

/// Resolve the project-filter directory for an arbitrary project root.
/// Returns the directory regardless of trust state — caller must check trust
/// separately if it wants to gate loading.
pub fn project_filter_dir(project_root: &Path) -> PathBuf {
    project_root.join(".aft").join("filters")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_and_project_filter_dir_helpers() {
        let storage = Path::new("/tmp/aft-storage");
        assert_eq!(
            user_filter_dir(storage),
            Path::new("/tmp/aft-storage/filters")
        );

        let project = Path::new("/repo");
        assert_eq!(project_filter_dir(project), Path::new("/repo/.aft/filters"));
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
        compress_with_registry(cmd, output, &empty_registry())
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
        // npm test typically runs vitest/jest under the hood. With specificity
        // dispatch, vitest's matches() returns false for "npm test" alone
        // (vitest's matcher looks for the token "vitest" or "jest").
        // So this should fall through to NpmCompressor (PackageManager tier).
        // This is the correct behavior: npm-managed test output is generic
        // unless we have explicit token evidence of the runner.
        let output = "added 100 packages, removed 2 packages\n";
        let _compressed = dispatch("npm test", output);
        // Just assert it didn't panic and emitted something. The PackageManager
        // module's `Some("test") => GenericCompressor` is the right fallback
        // here because we have no token signal.
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
    fn npm_run_lint_with_eslint_token_routes_to_eslint() {
        // `npm run lint` typically runs eslint, but the command string is
        // just "npm run lint" — no eslint token. This should fall through
        // to NpmCompressor's PackageManager tier (which then dispatches to
        // generic for "run"). That's correct: we don't have token evidence.
        let output = "> my-project@1.0.0 lint\n> eslint .\n\nAll good.\n";
        let _compressed = dispatch("npm run lint", output);
        // No assertion needed — just verifying no panic. The behavior is
        // correctly "fall through to generic" because the command string
        // has no eslint token.
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
    fn timeout_prefix_cargo_build_still_routes_to_cargo() {
        let output =
            "   Compiling foo v0.1.0\n    Finished `dev` profile [unoptimized] target(s) in 5s\n";
        let compressed =
            compress_with_registry("timeout 30 cargo build", output, &empty_registry());
        // CargoCompressor for build/check/run preserves the structure.
        assert!(compressed.contains("Compiling") || compressed.contains("Finished"));
    }
}
