//! TypeScript 7 projects are served by the native compiler's own language
//! server (`tsc --lsp --stdio`), because typescript-language-server needs the
//! `lib/tsserver.js` that TypeScript 7 no longer ships.
//!
//! The real-server tests need an installed TypeScript 7 and are opt-in:
//! - `AFT_TEST_TS7_NODE_MODULES`: a `node_modules` directory holding
//!   `typescript@7` and its `@typescript/typescript-<os>-<cpu>` package, for
//!   example from `bun add -d typescript@7` in a scratch directory.
//! - `AFT_TEST_TS5_NODE_MODULES`: a `node_modules` directory holding
//!   `typescript@5` and `typescript-language-server`, for the mixed-version
//!   test.
//!
//! CI installs both with `scripts/install-typescript-test-servers.sh`. A set
//! variable that points at a missing or wrong install fails the test instead
//! of skipping it.
//!
//! Fixture projects link `node_modules/typescript` to the shared install, the
//! way Bun's isolated linker and pnpm do, so the native binary must be found
//! from the package's real path.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::helpers::{link_cached_executable, AftProcess};

const TS7_ENV: &str = "AFT_TEST_TS7_NODE_MODULES";
const TS5_ENV: &str = "AFT_TEST_TS5_NODE_MODULES";

/// The install named by `env`, or `None` when the variable is unset. A set
/// variable that does not point at the expected install fails the test: CI
/// sets it, and a broken install step must not turn into a silent skip.
fn configured(env: &str) -> Option<PathBuf> {
    let modules = std::env::var_os(env).map(PathBuf::from)?;
    let package_json = modules.join("typescript").join("package.json");
    let version = fs::read_to_string(&package_json)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|json| json["version"].as_str().map(str::to_owned))
        .unwrap_or_else(|| {
            panic!(
                "{env} is set but {} is missing or unreadable",
                package_json.display()
            )
        });
    let expected_major = if env == TS7_ENV { "7." } else { "5." };
    assert!(
        version.starts_with(expected_major),
        "{env} holds typescript {version}, expected {expected_major}x"
    );
    if env == TS5_ENV {
        assert!(
            modules
                .join(".bin")
                .join("typescript-language-server")
                .exists(),
            "{env} has no .bin/typescript-language-server"
        );
    }
    Some(modules)
}

fn installed(env: &str, test: &str) -> Option<PathBuf> {
    let modules = configured(env);
    if modules.is_none() {
        eprintln!("SKIP {test}: {env} not set (see the module comment)");
    }
    modules
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// A package with its own tsconfig and a `node_modules/typescript` link to
/// the shared install.
fn typescript_package(dir: &Path, modules: &Path) {
    write(
        &dir.join("package.json"),
        r#"{"name":"fixture","private":true}"#,
    );
    // No `strict`: TypeScript 7 turns strict on by default and TypeScript 5
    // does not, which is how the mixed test tells the compilers apart.
    write(
        &dir.join("tsconfig.json"),
        r#"{"compilerOptions":{"noEmit":true,"target":"es2022","module":"esnext"},"include":["src"]}"#,
    );
    fs::create_dir_all(dir.join("node_modules")).unwrap();
    std::os::unix::fs::symlink(
        modules.join("typescript"),
        dir.join("node_modules").join("typescript"),
    )
    .unwrap();
}

fn spawn_configured(root: &Path, lsp_paths_extra: &[PathBuf]) -> AftProcess {
    spawn_configured_with_tmp(root, lsp_paths_extra, &root.join(".typescript-server-tmp"))
}

/// `tmp` becomes TMPDIR for AFT and, through it, the TypeScript server.
///
/// On macOS typescript-language-server probes case sensitivity in a shared
/// typescript-language-server<uid> temp directory using existsSync + mkdirSync.
/// Concurrent first starts can race to create it, so each fixture gets its own
/// temp directory as well as its own workspace and HOME. On macOS AFT also
/// treats TMPDIR as the temp root a standalone call may use without an
/// outside-access ask, so a test that reads a sibling of `root` must pass a
/// `tmp` that contains that sibling.
fn spawn_configured_with_tmp(root: &Path, lsp_paths_extra: &[PathBuf], tmp: &Path) -> AftProcess {
    fs::create_dir_all(tmp).unwrap();
    let mut aft = AftProcess::spawn_with_env(&[("TMPDIR", tmp.as_os_str())]);
    let configure = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "lsp_paths_extra": lsp_paths_extra,
            // Keep the assertions about the TypeScript servers: linters that
            // happen to be on PATH would add their own gaps.
            "config": super::helpers::user_config(json!({
                "lsp": { "disabled": ["biome", "oxlint"] }
            })),
        })
        .to_string(),
    );
    assert_eq!(configure["success"], true, "configure: {configure}");
    aft
}

fn lsp_diagnostics(aft: &mut AftProcess, file: &Path) -> serde_json::Value {
    let response = aft.send(
        &json!({
            "id": "diag",
            "command": "lsp_diagnostics",
            "file": file,
            "wait_ms": 10_000,
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "lsp_diagnostics: {response}");
    response
}

fn codes(diagnostics: &serde_json::Value) -> Vec<String> {
    diagnostics
        .as_array()
        .unwrap_or_else(|| panic!("diagnostics array: {diagnostics}"))
        .iter()
        .filter(|diagnostic| diagnostic["severity"] == "error")
        .map(|diagnostic| diagnostic["code"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn server_ids(response: &serde_json::Value) -> Vec<String> {
    response["lsp_servers_used"]
        .as_array()
        .unwrap_or_else(|| panic!("lsp_servers_used: {response}"))
        .iter()
        .map(|entry| {
            format!(
                "{}={}",
                entry["server_id"].as_str().unwrap(),
                entry["status"]
            )
        })
        .collect()
}

fn runtime_notes(aft: &mut AftProcess, file: &Path) -> Vec<String> {
    let response =
        aft.send(&json!({"id": "inspect-lsp", "command": "lsp_inspect", "file": file}).to_string());
    assert_eq!(response["success"], true, "lsp_inspect: {response}");
    serde_json::from_value(response["lsp_runtime_notes"].clone()).unwrap()
}

#[test]
fn typescript_server_temp_directories_are_isolated() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().canonicalize().unwrap();
    let bin = parent.join("bin");
    let server = bin.join("typescript-language-server");
    write(
        &server,
        "#!/bin/sh\nprintf 'server TMPDIR=%s\\n' \"$TMPDIR\" >&2\nexit 1\n",
    );
    fs::set_permissions(&server, fs::Permissions::from_mode(0o755)).unwrap();

    for name in ["first", "second"] {
        let root = parent.join(name);
        let file = root.join("src/main.ts");
        write(&root.join("tsconfig.json"), "{}");
        write(&file, "export const value = 1;\n");
        let mut aft = spawn_configured(&root, std::slice::from_ref(&bin));
        let _ = aft.send(
            &json!({"id": "probe", "command": "inspect", "scope": file, "sections": ["diagnostics"]})
                .to_string(),
        );
        let (_, stderr) = aft.stderr_output();
        assert!(
            stderr.contains(&format!(
                "server TMPDIR={}",
                root.join(".typescript-server-tmp").display()
            )),
            "server did not inherit its fixture's private temp directory: {stderr}"
        );
    }
}

#[test]
fn typescript_5_inspect_resolves_module_created_by_aft_write() {
    let Some(ts5) = installed(
        TS5_ENV,
        "typescript_5_inspect_resolves_module_created_by_aft_write",
    ) else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    typescript_package(&root, &ts5);
    write(
        &root.join("tsconfig.json"),
        r#"{"compilerOptions":{"noEmit":true,"target":"es2022","module":"esnext","moduleResolution":"bundler","allowImportingTsExtensions":true},"include":["src","lib"]}"#,
    );
    let importer = root.join("lib/use.ts");
    write(&importer, "import { scan } from '../src/scanner2.ts';\nexport const value: number = scan(1);\nexport const control: number = 'positive control';\n");
    // AftProcess disables the OS project watcher, so resolution must come from
    // the write notification rather than a later filesystem event.
    let mut aft = spawn_configured(&root, &[ts5.join(".bin")]);
    let inspect = |aft: &mut AftProcess| {
        let response = aft.send(&json!({"id": "inspect-importer", "command": "inspect", "scope": importer, "sections": ["diagnostics"]}).to_string());
        assert_eq!(response["success"], true, "{response}");
        response
    };
    let before = inspect(&mut aft);
    assert_eq!(before["summary"]["diagnostics"]["errors"], 2, "{before}");
    assert!(
        before["details"]["diagnostics"]
            .to_string()
            .contains("Cannot find module"),
        "{before}"
    );
    let written = aft.send(&json!({"id": "write-module", "command": "write", "file": root.join("src/scanner2.ts"), "content": "export function scan(n: number): number { return n + 1; }\n"}).to_string());
    assert_eq!(written["success"], true, "{written}");

    // Keep the independent assignment error as a positive control: an empty
    // or missing diagnostics report must not look like successful resolution.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let after = inspect(&mut aft);
        let messages = after["details"]["diagnostics"].to_string();
        if after["summary"]["diagnostics"]["errors"] == 1
            && !messages.contains("Cannot find module")
            && messages.contains("not assignable")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "importer stayed stale after AFT write: {after}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn typescript_5_inspect_outside_project_root_uses_external_workspace() {
    let Some(ts5) = installed(
        TS5_ENV,
        "typescript_5_inspect_outside_project_root_uses_external_workspace",
    ) else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().canonicalize().unwrap();
    let root = parent.join("configured");
    fs::create_dir_all(&root).unwrap();
    let external = parent.join("external");
    typescript_package(&external, &ts5);
    let broken = external.join("src/broken.ts");
    write(&broken, "export const broken: number = \"not a number\";\n");
    // The external workspace is outside the project root, so on macOS it is
    // readable without an ask only because it sits under AFT's TMPDIR. This
    // fixture's own temp directory still gives its server a private temp dir.
    let mut aft = spawn_configured_with_tmp(&root, &[ts5.join(".bin")], &parent);
    let response = aft.send(
        &json!({
            "id": "inspect-external", "command": "inspect", "scope": broken,
            "sections": ["diagnostics"],
        })
        .to_string(),
    );
    // The response only quotes the first stderr line on a startup crash. Drain
    // AFT's exit logs too so a failed assertion includes the Node error and path.
    let (_, stderr) = aft.stderr_output();
    assert_eq!(
        response["success"], true,
        "{response}\nAFT stderr:\n{stderr}"
    );
    assert_eq!(
        response["scope_files"], 1,
        "{response}\nAFT stderr:\n{stderr}"
    );
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response}\nAFT stderr:\n{stderr}"
    );
    assert_eq!(
        response["summary"]["diagnostics"]["by_producer"]["typescript"]["errors"], 1,
        "{response}\nAFT stderr:\n{stderr}"
    );
    assert!(
        response["details"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["file"] == broken.to_str().unwrap()
                && row["message"].as_str().is_some_and(
                    |message| message.contains("2322") || message.contains("not assignable")
                )),
        "{response}\nAFT stderr:\n{stderr}"
    );
}

#[test]
fn typescript_7_project_is_served_by_the_native_language_server() {
    let Some(ts7) = installed(
        TS7_ENV,
        "typescript_7_project_is_served_by_the_native_language_server",
    ) else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("app");
    typescript_package(&root, &ts7);
    let broken = root.join("src").join("broken.ts");
    write(&broken, "export const count: number = \"wrong\";\n");
    let edited = root.join("src").join("edited.ts");
    write(&edited, "export const total: number = 1;\n");
    write(
        &root.join("src").join("never.ts"),
        "export const never: number = \"two\";\n",
    );

    let mut aft = spawn_configured(&root, &[]);

    let diagnostics = lsp_diagnostics(&mut aft, &broken);
    assert_eq!(
        codes(&diagnostics["diagnostics"]),
        vec!["2322"],
        "{diagnostics}"
    );
    assert!(
        server_ids(&diagnostics)
            .iter()
            .any(|entry| entry == "typescript-native=\"pull_ok\""),
        "{diagnostics}"
    );
    assert!(
        !server_ids(&diagnostics)
            .iter()
            .any(|entry| entry.starts_with("typescript=")),
        "typescript-language-server must not be tried: {diagnostics}"
    );

    let inspect = aft.send(
        &json!({
            "id": "inspect",
            "command": "inspect",
            "scope": broken,
            "sections": ["diagnostics"],
        })
        .to_string(),
    );
    assert_eq!(inspect["success"], true, "inspect: {inspect}");
    let summary = &inspect["summary"]["diagnostics"];
    assert_eq!(summary["errors"], 1, "inspect: {inspect}");
    assert_eq!(
        summary["by_producer"]["typescript-native"]["errors"], 1,
        "inspect: {inspect}"
    );

    let edit = aft.send(
        &json!({
            "id": "edit",
            "command": "edit_match",
            "file": edited,
            "match": "= 1;",
            "replacement": "= \"one\";",
            "diagnostics": true,
            "wait_ms": 10_000,
        })
        .to_string(),
    );
    assert_eq!(edit["success"], true, "edit: {edit}");
    assert_eq!(edit["lsp_complete"], true, "edit: {edit}");
    assert_eq!(
        codes(&edit["lsp_diagnostics"]),
        vec!["2322"],
        "edit: {edit}"
    );

    let notes = runtime_notes(&mut aft, &broken);
    assert!(
        notes
            .iter()
            .any(|note| note.contains(": native language server (project installation)")),
        "{notes:?}"
    );

    // A scoped inspect opens and pulls a file nothing opened before: the
    // native server has no workspace-wide pull and publishes no source-file
    // diagnostics unasked.
    let inspect = aft.send(
        &json!({
            "id": "inspect-unopened",
            "command": "inspect",
            "scope": root.join("src").join("never.ts"),
            "sections": ["diagnostics"],
        })
        .to_string(),
    );
    assert_eq!(inspect["success"], true, "inspect: {inspect}");
    let summary = &inspect["summary"]["diagnostics"];
    assert_eq!(
        summary["by_producer"]["typescript-native"]["errors"], 1,
        "inspect: {inspect}"
    );
    assert!(
        summary.get("not_applicable").is_none(),
        "the typescript marker is served by the native server: {inspect}"
    );
    assert!(aft.shutdown().success());
}

/// One project, one TypeScript 5 package and one TypeScript 7 package. Each
/// gets its own server, and each file is checked by its own compiler: a
/// parameter without a type is an error only under TypeScript 7, which is
/// strict by default.
#[test]
fn mixed_typescript_5_and_7_packages_get_separate_servers() {
    let test = "mixed_typescript_5_and_7_packages_get_separate_servers";
    let (Some(ts7), Some(ts5)) = (installed(TS7_ENV, test), installed(TS5_ENV, test)) else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("monorepo");
    write(&root.join("package.json"), r#"{"private":true}"#);
    let source =
        "export function echo(value) { return value; }\nexport const count: number = \"wrong\";\n";
    typescript_package(&root.join("packages").join("legacy"), &ts5);
    typescript_package(&root.join("packages").join("next"), &ts7);
    let legacy = root
        .join("packages")
        .join("legacy")
        .join("src")
        .join("index.ts");
    let next = root
        .join("packages")
        .join("next")
        .join("src")
        .join("index.ts");
    write(&legacy, source);
    write(&next, source);

    let mut aft = spawn_configured(&root, &[ts5.join(".bin")]);

    // lsp_diagnostics starts the TypeScript 5 server and opens the file.
    // typescript-language-server 6 publishes without a document version, so
    // lsp_diagnostics cannot prove that push fresh (a limitation of the push
    // path, not of the server choice); read the stored report instead.
    let legacy_diagnostics = lsp_diagnostics(&mut aft, &legacy);
    assert!(
        server_ids(&legacy_diagnostics)
            .iter()
            .any(|entry| entry.starts_with("typescript=")),
        "{legacy_diagnostics}"
    );
    assert!(
        !server_ids(&legacy_diagnostics)
            .iter()
            .any(|entry| entry.starts_with("typescript-native=")),
        "{legacy_diagnostics}"
    );
    let mut stored = serde_json::Value::Null;
    for _ in 0..80 {
        stored = aft.send(
            &json!({"id": "inspect-lsp", "command": "lsp_inspect", "file": legacy}).to_string(),
        );
        if stored["diagnostics_count"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    assert_eq!(codes(&stored["diagnostics"]), vec!["2322"], "{stored}");
    assert_eq!(
        stored["diagnostics"][0]["source"], "typescript",
        "typescript-language-server labels its diagnostics 'typescript': {stored}"
    );

    let next_diagnostics = lsp_diagnostics(&mut aft, &next);
    let mut next_codes = codes(&next_diagnostics["diagnostics"]);
    next_codes.sort();
    assert_eq!(next_codes, vec!["2322", "7006"], "{next_diagnostics}");
    assert!(
        server_ids(&next_diagnostics)
            .iter()
            .any(|entry| entry == "typescript-native=\"pull_ok\""),
        "{next_diagnostics}"
    );
    assert!(
        !server_ids(&next_diagnostics)
            .iter()
            .any(|entry| entry.starts_with("typescript=")),
        "{next_diagnostics}"
    );

    // A scoped inspect over both files counts each under its own server.
    let inspect = aft.send(
        &json!({
            "id": "inspect",
            "command": "inspect",
            "scope": [legacy, next],
            "sections": ["diagnostics"],
        })
        .to_string(),
    );
    assert_eq!(inspect["success"], true, "inspect: {inspect}");
    let by_producer = &inspect["summary"]["diagnostics"]["by_producer"];
    assert_eq!(by_producer["typescript"]["errors"], 1, "inspect: {inspect}");
    assert_eq!(
        by_producer["typescript-native"]["errors"], 2,
        "inspect: {inspect}"
    );

    let notes = runtime_notes(&mut aft, &next);
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("TypeScript 5.") && note.contains("project installation")),
        "{notes:?}"
    );
    assert!(
        notes.iter().any(
            |note| note.starts_with("TypeScript 7.") && note.contains("native language server")
        ),
        "{notes:?}"
    );
    assert!(aft.shutdown().success());
}

/// A marker script that records that it ran; nothing in these tests may run
/// it.
fn sentinel(path: &Path, marker: &Path) {
    let source = format!("#!/bin/sh\necho ran >> '{}'\nexit 1\n", marker.display());
    link_cached_executable(path, source.as_bytes()).unwrap();
}

/// A TypeScript 7 installation without its platform package (optional
/// dependencies skipped, or an unsupported platform) is a named gap. Neither
/// typescript-language-server, which is known to fail on it, nor the Node
/// wrapper `node_modules/.bin/tsc` is started.
#[test]
fn typescript_7_without_its_platform_package_is_a_named_gap_and_spawns_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("app");
    write(&root.join("package.json"), r#"{"private":true}"#);
    write(&root.join("tsconfig.json"), "{}");
    let package_dir = root.join("node_modules").join("typescript");
    match configured(TS7_ENV) {
        // A real install, copied without the platform package.
        Some(modules) => copy_dir(&modules.join("typescript"), &package_dir),
        None => write(
            &package_dir.join("package.json"),
            r#"{"name":"typescript","version":"7.0.2","bin":{"tsc":"./bin/tsc"}}"#,
        ),
    }
    let marker = temp.path().join("spawned");
    let bin = root.join("node_modules").join(".bin");
    sentinel(&bin.join("tsc"), &marker);
    sentinel(&bin.join("typescript-language-server"), &marker);
    let file = root.join("src").join("index.ts");
    write(&file, "export const count: number = \"wrong\";\n");

    let mut aft = spawn_configured(&root, &[]);
    let diagnostics = lsp_diagnostics(&mut aft, &file);
    let statuses = server_ids(&diagnostics).join("\n");
    assert!(
        statuses.contains("typescript-native=\"spawn_failed")
            && statuses.contains("TypeScript unavailable: this project uses TypeScript 7.")
            && statuses.contains("@typescript/typescript-")
            && statuses.contains("was not found from"),
        "{diagnostics}"
    );
    assert!(!statuses.contains("bun install"), "{diagnostics}");
    assert!(!statuses.contains("typescript="), "{diagnostics}");

    let inspect = aft.send(
        &json!({
            "id": "inspect",
            "command": "inspect",
            "scope": file,
            "sections": ["diagnostics"],
        })
        .to_string(),
    );
    assert_eq!(inspect["success"], true, "inspect: {inspect}");
    let gaps = inspect["details"]["diagnostics_gaps"].to_string() + &inspect.to_string();
    assert!(
        gaps.contains("typescript-native") && gaps.contains("@typescript/typescript-"),
        "inspect: {inspect}"
    );

    assert!(aft.shutdown().success());
    assert!(
        !marker.exists(),
        "a TypeScript server program was started: {}",
        fs::read_to_string(&marker).unwrap_or_default()
    );
}

/// The native server is found by package layout, so a binary that does not
/// identify itself as typescript-go after initialize is stopped and named.
#[test]
fn native_binary_that_is_not_typescript_go_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("app");
    write(&root.join("package.json"), r#"{"private":true}"#);
    write(&root.join("tsconfig.json"), "{}");
    let modules = root.join("node_modules");
    write(
        &modules.join("typescript").join("package.json"),
        r#"{"name":"typescript","version":"7.0.2"}"#,
    );
    let platform = format!(
        "typescript-{}-{}",
        match std::env::consts::OS {
            "macos" => "darwin",
            other => other,
        },
        match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "x64",
            other => other,
        }
    );
    let platform_dir = modules.join("@typescript").join(platform);
    write(&platform_dir.join("package.json"), "{}");
    // Any other language server in the platform binary's place: the fake
    // server reports serverInfo.name "fake-lsp-server".
    let binary = platform_dir.join("lib").join("tsc");
    let source = format!("#!/bin/sh\nexec '{}'\n", fake_server_path().display());
    link_cached_executable(&binary, source.as_bytes()).unwrap();
    let file = root.join("src").join("index.ts");
    write(&file, "export const count = 1;\n");

    let mut aft = spawn_configured(&root, &[]);
    let diagnostics = lsp_diagnostics(&mut aft, &file);
    let statuses = server_ids(&diagnostics).join("\n");
    assert!(
        statuses.contains("typescript-native=\"spawn_failed")
            && statuses
                .contains("identified itself as \\\"fake-lsp-server\\\" rather than typescript-go"),
        "{diagnostics}"
    );
    assert!(aft.shutdown().success());
}

fn fake_server_path() -> PathBuf {
    crate::test_helpers::fake_lsp::fake_server_binary()
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Unscoped inspect observes diagnostics already collected, without opening
/// every TypeScript file. For an empty working set, successful initialization
/// certifies that empty result, unlike Rust's whole-workspace compiler check.
#[test]
fn unscoped_typescript_5_inspect_keeps_the_empty_working_set_contract() {
    let Some(ts5) = installed(
        TS5_ENV,
        "unscoped_typescript_5_inspect_keeps_the_empty_working_set_contract",
    ) else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("app");
    typescript_package(&root, &ts5);
    write(
        &root.join("src/clean.ts"),
        "export const answer: number = 42;\n",
    );
    let mut aft = spawn_configured(&root, &[ts5.join(".bin")]);
    let response = aft.send(
        &json!({
            "id": "real-ts5-unscoped", "command": "inspect", "sections": "diagnostics"
        })
        .to_string(),
    );
    eprintln!("real TypeScript 5 unscoped: {response:#}");
    assert_eq!(response["success"], true, "{response:#}");
    assert!(
        response["wait_stamp"]["phases"]
            .as_array()
            .expect("completed phases")
            .iter()
            .any(|phase| phase["id"] == "lsp_start" && phase["producer"] == "typescript"),
        "{response:#}"
    );
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 0,
        "{response:#}"
    );
    assert_ne!(
        response["summary"]["diagnostics"]["complete"], false,
        "{response:#}"
    );
    assert!(
        !response["gaps"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|gap| gap["categories"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|category| category == "diagnostics")),
        "{response:#}"
    );
}
