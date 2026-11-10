//! `restrict_to_project_root` is the user-facing spelling of the policy's
//! "outside roots: deny" rule. These tests pin that a configuration written
//! before the rule existed keeps every outcome it had: the same allowed path,
//! the same refusal code and the same refusal text, for reads, mutations and
//! preserved-final-component writes, on every ask route, with and without the
//! per-request restriction an untrusted subc consumer's calls force onto
//! themselves (`AppContext::force_restrict_guard`). The table also covers a
//! linked worktree reading its main checkout, and `aft_search` given a Git
//! repository other than the project root.
//!
//! The expected outcomes live in `fixtures/outside_roots_legacy_outcomes.txt`.
//! They were recorded from the engine as it decided before the setting was
//! read through the policy rule, so a difference is a behaviour change for an
//! existing configuration, not a fixture to refresh.

// The outcome table is Unix-only: it includes symlink probes and Unix path
// spellings. On other targets only the rule test runs, so the table helpers
// are unused there.
#![cfg_attr(not(unix), allow(dead_code))]
use super::*;
use crate::config::Config;
use crate::config_resolve::{resolve_config, ConfigTier};
use crate::context::AppContext;

/// The `aft.jsonc` user documents existing installations have: the key
/// absent, explicitly off, and on.
const EXISTING_CONFIGS: [(&str, &str); 3] = [
    ("absent", "{}"),
    ("false", r#"{ "restrict_to_project_root": false }"#),
    ("true", r#"{ "restrict_to_project_root": true }"#),
];

const LEGACY_OUTCOMES: &str = include_str!("fixtures/outside_roots_legacy_outcomes.txt");

struct Fixture {
    dir: tempfile::TempDir,
    root: PathBuf,
    outside: PathBuf,
    temp: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        // Under the crate directory, not the system temp directory, so the
        // standalone temp exemption only reaches the directory this fixture
        // names as temp.
        let dir = tempfile::tempdir_in(crate::test_support::manifest_dir()).unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("root");
        let outside = base.join("outside");
        let temp = base.join("tmp");
        for path in [&root, &outside, &temp] {
            std::fs::create_dir(path).unwrap();
        }
        std::fs::write(root.join("inside.txt"), "inside").unwrap();
        std::fs::write(outside.join("outside.txt"), "outside").unwrap();
        std::fs::write(outside.join("floor.txt"), "floor").unwrap();
        std::fs::write(temp.join("scratch.txt"), "scratch").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("outside.txt"), root.join("link.txt")).unwrap();
            std::os::unix::fs::symlink(&outside, root.join("link-dir")).unwrap();
        }
        Self {
            dir,
            root,
            outside,
            temp,
        }
    }

    fn base(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// Resolve a user `aft.jsonc` document through the real config resolver,
    /// then point the resulting context at this fixture.
    fn context(&self, doc: &str, project_root: bool) -> AppContext {
        AppContext::new(
            Box::new(crate::language::StubProvider),
            self.config(doc, project_root.then(|| self.root.clone())),
        )
    }

    fn config(&self, doc: &str, project_root: Option<PathBuf>) -> Config {
        let resolved = resolve_config(&[ConfigTier {
            tier: "user".into(),
            source: "/fixture/user/aft.jsonc".into(),
            doc: doc.into(),
        }]);
        assert!(resolved.dropped.is_empty(), "{:?}", resolved.dropped);
        let mut config: Config = resolved.config;
        config.project_root = project_root;
        config.storage_dir = Some(self.base().join("store"));
        config.backup.enabled = Some(false);
        config.sandbox.read_deny = vec![self.outside.join("floor.txt")];
        config
    }

    /// A real linked worktree: `main/` is a repository with one commit and
    /// `linked/` was added from it with `git worktree add`. A context rooted
    /// at `linked/` that records `main/.git` as its common Git directory gets
    /// the standalone exemption that lets it read `main/`, while outside
    /// paths ask; under outside roots: deny the exemption does not apply.
    fn linked_context(&self, doc: &str) -> (AppContext, PathBuf, PathBuf) {
        let main = self.base().join("main");
        let linked = self.base().join("linked");
        if !linked.exists() {
            std::fs::create_dir(&main).unwrap();
            std::fs::write(main.join("tracked.txt"), "tracked").unwrap();
            git(&main, &["init", "-q"]);
            git(&main, &["add", "tracked.txt"]);
            git(&main, &["commit", "-q", "-m", "fixture"]);
            git(&main, &["worktree", "add", "-q", "../linked"]);
        }
        let ctx = AppContext::new(
            Box::new(crate::language::StubProvider),
            self.config(doc, Some(linked.clone())),
        );
        ctx.set_cache_role(true, Some(main.join(".git")));
        (ctx, main, linked)
    }

    /// A context for `aft_search` (`semantic_search`) rooted at `root/`, with
    /// a Git repository nested inside the root and another one outside it.
    /// An allowed external search walks the selected repository.
    fn search_context(&self, doc: &str) -> (AppContext, PathBuf, PathBuf) {
        let nested = self.root.join("nested");
        let outside_repo = self.base().join("outside-repo");
        for repo in [&nested, &outside_repo] {
            if !repo.exists() {
                std::fs::create_dir(repo).unwrap();
                std::fs::write(repo.join("marker.rs"), "fn nested_marker() {}\n").unwrap();
                git(repo, &["init", "-q"]);
            }
        }
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            self.config(doc, Some(self.root.clone())),
        );
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            crate::context::SemanticIndexStatus::Disabled;
        crate::data_layer::surfaces::serve_text_index_for_test(&ctx);
        (ctx, nested, outside_repo)
    }

    /// Paths each configuration is asked about. Relative probes only run when
    /// a project root is configured; without one they would resolve against
    /// the test runner's working directory.
    fn probes(&self, project_root: bool) -> Vec<(&'static str, PathBuf)> {
        let mut probes = vec![
            ("inside-absolute", self.root.join("inside.txt")),
            ("inside-missing-absolute", self.root.join("new.txt")),
            ("outside-existing", self.outside.join("outside.txt")),
            ("outside-missing", self.outside.join("missing.txt")),
            ("outside-floor", self.outside.join("floor.txt")),
            ("temp-scratch", self.temp.join("scratch.txt")),
        ];
        if project_root {
            probes.extend([
                ("inside-relative", PathBuf::from("inside.txt")),
                ("inside-missing-relative", PathBuf::from("new.txt")),
                ("dotdot-escape", PathBuf::from("../outside/outside.txt")),
            ]);
            #[cfg(unix)]
            probes.extend([
                ("symlink-file-escape", PathBuf::from("link.txt")),
                ("symlink-dir-escape", PathBuf::from("link-dir/missing.txt")),
            ]);
        }
        probes
    }

    fn normalize(&self, text: &str) -> String {
        text.replace(&self.base().display().to_string(), "<dir>")
    }
}

/// Run git for a fixture with the host's global and system configuration
/// (and any injected `GIT_CONFIG_*` entries) ignored.
fn git(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=AFT test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_COUNT", "0")
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

#[derive(Clone, Copy)]
enum Route {
    Lowered,
    Plugin,
    Elicitation,
    Deny,
}

impl Route {
    const ALL: [Route; 4] = [
        Route::Lowered,
        Route::Plugin,
        Route::Elicitation,
        Route::Deny,
    ];

    fn label(self) -> &'static str {
        match self {
            Route::Lowered => "lowered",
            Route::Plugin => "plugin",
            Route::Elicitation => "elicitation",
            Route::Deny => "deny",
        }
    }

    fn install(self, owner: &std::sync::Arc<()>) -> AskRouteGuard {
        ask_route_guard(match self {
            Route::Lowered => None,
            Route::Plugin => Some(AskRoute::Plugin {
                scope: plugin::Scope::new(owner, "outside-roots-session"),
                once: Vec::new(),
            }),
            Route::Elicitation => Some(AskRoute::Elicitation { once: Vec::new() }),
            Route::Deny => Some(AskRoute::Deny),
        })
    }
}

const CHECKS: [&str; 3] = ["read", "mutate", "write-location"];

/// One validation outcome as a single line: the path a tool would touch, or
/// the refusal code and text. Ask refusals carry generated grant identifiers,
/// so only their code is kept.
fn outcome(f: &Fixture, ctx: &AppContext, forced: bool, check: &str, path: &Path) -> String {
    let req_id = "outside-roots";
    let _forced = forced.then(|| ctx.force_restrict_guard(req_id));
    let result = match check {
        "read" => ctx.validate_read_path(req_id, "outside-roots-session", path),
        "mutate" => ctx.validate_path(req_id, path),
        _ => ctx.validate_write_location(req_id, path),
    };
    match result {
        Ok(path) => format!("ok {}", f.normalize(&path.display().to_string())),
        Err(response) => {
            let code = response.data["code"].as_str().unwrap_or("?").to_string();
            if is_access_request(&response) {
                format!("ask {code}")
            } else {
                let message = response.data["message"].as_str().unwrap_or("");
                format!("err {code}: {}", f.normalize(message))
            }
        }
    }
}

/// Every outcome, one line per (root, route, forced, probe, check), with the
/// three existing configurations side by side.
fn outcome_table(f: &Fixture) -> Vec<(String, [String; 3])> {
    let owner = std::sync::Arc::new(());
    let _temp = temp::override_roots_for_test(vec![f.temp.clone()]);
    let mut rows = Vec::new();
    for project_root in [true, false] {
        let contexts = EXISTING_CONFIGS.map(|(_, doc)| f.context(doc, project_root));
        for route in Route::ALL {
            let _route = route.install(&owner);
            for forced in [false, true] {
                for (probe, path) in f.probes(project_root) {
                    for check in CHECKS {
                        let key = format!(
                            "root={} route={} forced={forced} {probe} {check}",
                            if project_root { "set" } else { "none" },
                            route.label(),
                        );
                        let outcomes = [0, 1, 2]
                            .map(|index| outcome(f, &contexts[index], forced, check, &path));
                        rows.push((key, outcomes));
                    }
                }
            }
        }
    }
    // A linked worktree's main checkout: readable without an ask on a
    // standalone route while outside paths ask, refused like any other outside
    // path under outside roots: deny.
    let linked = EXISTING_CONFIGS.map(|(_, doc)| f.linked_context(doc));
    let (_, main, linked_root) = &linked[0];
    let linked_probes = [
        ("main-checkout-file", main.join("tracked.txt")),
        ("main-checkout-missing", main.join("new.txt")),
        ("linked-inside", linked_root.join("tracked.txt")),
    ];
    for route in Route::ALL {
        let _route = route.install(&owner);
        for forced in [false, true] {
            for (probe, path) in &linked_probes {
                for check in CHECKS {
                    let key = format!(
                        "root=linked route={} forced={forced} {probe} {check}",
                        route.label(),
                    );
                    let outcomes =
                        [0, 1, 2].map(|index| outcome(f, &linked[index].0, forced, check, path));
                    rows.push((key, outcomes));
                }
            }
        }
    }
    // `aft_search` with a path naming a Git repository other than the
    // project root: one nested inside the root (refused by the search's own
    // root check) and one outside it (refused by path validation; the search
    // then rewrites the refusal message to name the resolved Git root).
    let search = EXISTING_CONFIGS.map(|(_, doc)| f.search_context(doc));
    let (_, nested, outside_repo) = &search[0];
    let search_probes = [
        ("nested-git-root", nested.clone()),
        ("outside-git-root", outside_repo.clone()),
    ];
    for route in Route::ALL {
        let _route = route.install(&owner);
        for forced in [false, true] {
            for (probe, path) in &search_probes {
                let key = format!(
                    "search route={} forced={forced} {probe} aft_search",
                    route.label(),
                );
                let outcomes =
                    [0, 1, 2].map(|index| search_outcome(f, &search[index].0, forced, path));
                rows.push((key, outcomes));
            }
        }
    }
    rows
}

/// One `aft_search` outcome as a single line: the external root, how the
/// request was interpreted and the result file paths, or the refusal code
/// and text.
fn search_outcome(f: &Fixture, ctx: &AppContext, forced: bool, path: &Path) -> String {
    let req_id = "outside-roots";
    let _forced = forced.then(|| ctx.force_restrict_guard(req_id));
    let request = crate::protocol::RawRequest {
        id: req_id.into(),
        command: "semantic_search".into(),
        session_id: Some("outside-roots-session".into()),
        lsp_hints: None,
        params: serde_json::json!({"pattern": "nested_marker", "path": path}),
    };
    let response = crate::commands::semantic_search::handle_semantic_search(&request, ctx);
    if is_access_request(&response) {
        return format!("ask {}", response.data["code"].as_str().unwrap_or("?"));
    }
    let value = serde_json::to_value(&response).unwrap();
    if value["success"] == true {
        let mut files = value["results"]
            .as_array()
            .map(|results| {
                results
                    .iter()
                    .map(|result| f.normalize(result["file"].as_str().unwrap_or("?")))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        files.sort();
        format!(
            "ok external_root={} interpreted_as={} results={files:?}",
            f.normalize(value["external_root"].as_str().unwrap_or("-")),
            value["interpreted_as"].as_str().unwrap_or("-"),
        )
    } else {
        format!(
            "err {}: {}",
            value["code"].as_str().unwrap_or("?"),
            f.normalize(value["message"].as_str().unwrap_or(""))
        )
    }
}

fn render(rows: &[(String, [String; 3])]) -> String {
    let mut out = String::new();
    for (key, outcomes) in rows {
        out.push_str(key);
        out.push('\n');
        for ((label, _), outcome) in EXISTING_CONFIGS.iter().zip(outcomes) {
            out.push_str(&format!("  {label}: {outcome}\n"));
        }
    }
    out
}

#[cfg(unix)]
#[test]
fn outside_roots_existing_configs_keep_their_legacy_outcomes() {
    let f = Fixture::new();
    let rows = outcome_table(&f);
    let rendered = render(&rows);
    // Writes the live table to the named file, so a reviewer can diff it
    // against the recorded one.
    if let Some(path) = std::env::var_os("AFT_RECORD_OUTSIDE_ROOTS_OUTCOMES") {
        std::fs::write(path, &rendered).unwrap();
    }
    let expected = LEGACY_OUTCOMES.lines().collect::<Vec<_>>();
    let actual = rendered.lines().collect::<Vec<_>>();
    let first_difference = expected
        .iter()
        .zip(&actual)
        .position(|(expected, actual)| expected != actual);
    assert!(
        first_difference.is_none() && expected.len() == actual.len(),
        "an existing configuration changed outcome near line {:?} (expected {} lines, got {}):\n{}",
        first_difference.map(|line| line + 1),
        expected.len(),
        actual.len(),
        first_difference
            .map(|line| format!("expected: {}\nactual:   {}", expected[line], actual[line]))
            .unwrap_or_default()
    );
}

/// What "outside roots: deny" means, checked on the live outcomes rather than
/// against the recorded file: with the setting on and a project root, every
/// probe that leaves the root is refused with `path_outside_root` on every
/// route and every check, and never reaches an ask. With it absent or off the
/// outcomes are identical to each other.
#[cfg(unix)]
#[test]
fn outside_roots_setting_on_denies_every_path_outside_the_root() {
    let f = Fixture::new();
    for (key, [absent, off, on]) in outcome_table(&f) {
        assert_eq!(absent, off, "{key}");
        let leaves_root = [
            "outside-existing",
            "outside-missing",
            "outside-floor",
            "temp-scratch",
            "dotdot-escape",
            "symlink-file-escape",
            "symlink-dir-escape",
            "main-checkout-file",
            "main-checkout-missing",
            // A Git repository nested inside the configured project root:
            // `aft_search` treats its Git root as external and refuses it
            // under outside roots: deny.
            "nested-git-root",
            "outside-git-root",
        ]
        .iter()
        .any(|probe| key.contains(&format!(" {probe} ")));
        // A write location keeps its final component as spelled, so the
        // in-root link itself (not its outside target) is what is judged.
        let link_itself = key.contains(" symlink-file-escape write-location");
        let has_root = ["root=set", "root=linked", "search"]
            .iter()
            .any(|prefix| key.starts_with(prefix));
        if has_root && leaves_root && !link_itself {
            assert!(on.starts_with("err path_outside_root: "), "{key}: {on}");
        }
    }
}

/// The configured rule each existing document resolves to, and the decision
/// the policy built from it gives a path outside its root.
#[test]
fn outside_roots_existing_configs_resolve_to_the_policy_rule() {
    let f = Fixture::new();
    let outside = f.outside.join("outside.txt");
    for ((label, doc), rule) in
        EXISTING_CONFIGS
            .iter()
            .zip([OutsideRoots::Ask, OutsideRoots::Ask, OutsideRoots::Deny])
    {
        for project_root in [true, false] {
            let ctx = f.context(doc, project_root);
            let config = ctx.config();
            assert_eq!(OutsideRoots::from_config(&config), rule, "{label}");
            assert_eq!(
                OutsideRoots::for_request(&config, true),
                OutsideRoots::Deny,
                "{label}: a forced request denies whatever is configured"
            );
            let policy = AccessPolicy::from_config(
                &config,
                &f.base().join("store"),
                vec![f.root.clone()],
                &f.base().join("home"),
            );
            let expected = if rule == OutsideRoots::Deny && project_root {
                AccessDecision::Deny(AccessDenial::OutsideRoots)
            } else {
                AccessDecision::Ask
            };
            assert_eq!(
                policy.decide(AccessOperation::Read, &outside, false),
                expected,
                "{label} root={project_root}"
            );
        }
    }
}
