use super::*;

fn config(conn: &Connection, manifest: &mut Manifest, path: &str, source: &str) {
    let payload = join::CallgraphBlob::config(source.as_bytes(), "fixture")
        .to_bytes()
        .unwrap();
    let key = blake3::hash(&payload);
    conn.execute(
        "INSERT OR IGNORE INTO blob_payloads VALUES (?1, ?2)",
        params![key.as_bytes().as_slice(), payload],
    )
    .unwrap();
    *manifest = Manifest::new(
        manifest
            .entries()
            .filter(|(candidate, _)| candidate.as_bytes() != path.as_bytes())
            .map(|(path, entry)| (path.clone(), entry.clone())),
    )
    .unwrap();
    manifest
        .insert(
            RelPath::new(path.as_bytes()).unwrap(),
            ManifestEntry::Regular {
                mode: 0o100644,
                planes: RegularPlanes {
                    callgraph: Some(key.to_hex().to_string()),
                    semantic: None,
                },
                resolution_input: true,
            },
        )
        .unwrap();
}

fn workspace_fixture() -> Fixture {
    let mut f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    f.base = manifest(
        &conn,
        &[
            (
                "a.ts",
                "import { target } from 'pkg'; export function a() { return target(); }",
            ),
            (
                "b.ts",
                "import { target } from 'pkg'; export function b() { return target(); }",
            ),
            (
                "other.ts",
                "import { target } from './local'; export function other() { return target(); }",
            ),
            (
                "q.ts",
                "import { target } from 'q'; export function q() { return target(); }",
            ),
            ("packages/q/index.ts", "export function target() {}"),
            ("local.ts", "export function target() {}"),
            ("packages/pkg/one.ts", "export function target() {}"),
            ("packages/pkg/two.ts", "export function target() {}"),
        ],
    );
    config(
        &conn,
        &mut f.base,
        "package.json",
        r#"{"workspaces":["packages/*"],"version":"1"}"#,
    );
    config(
        &conn,
        &mut f.base,
        "packages/pkg/package.json",
        r#"{"name":"pkg","exports":"./one.ts"}"#,
    );
    config(
        &conn,
        &mut f.base,
        "packages/q/package.json",
        r#"{"name":"q","exports":"./index.ts"}"#,
    );
    f.next = f.base.clone();
    f
}

#[test]
fn memo_hit_callers_retain_member_name_consultations() {
    let f = workspace_fixture();
    let (base, _) = prepare(&f);
    let bindings = load_bindings(&Connection::open(base).unwrap()).unwrap();
    for caller in ["a.ts", "b.ts"] {
        let binding = &bindings[caller];
        assert!(
            binding
                .consulted_facts
                .contains(&("packages/pkg/package.json".into(), "name".into())),
            "{caller} lost member-name provenance on a memo hit"
        );
        assert!(binding
            .consulted_facts
            .contains(&("packages/pkg/package.json".into(), "exports".into())));
        assert!(!binding.unattributed, "{caller}: {binding:?}");
    }
    assert!(bindings["other.ts"].consulted_facts.is_empty());
}

#[test]
fn disk_facts_disable_recording_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let disk = crate::callgraph_store::disk_facts::DiskFacts::new(dir.path());
    use crate::callgraph_store::facts::ProjectFacts;
    assert!(!disk.records_config_facts());
    let paths = crate::callgraph_store::facts::FactPaths {
        root: dir.path(),
        facts: &disk,
    };
    paths.config_field(dir.path(), "package.json", "exports");
    disk.memo_start(dir.path(), "package", "pkg");
    disk.memo_finish(dir.path(), "package", "pkg");
    disk.memo_replay(dir.path(), "package", "pkg");
    assert!(disk.workspace_package(dir.path(), "pkg").is_none());
    assert!(disk.workspace_members(dir.path()).is_none());
}

fn check_transition(
    f: &Fixture,
    expected_dependents: usize,
    expected_resolved: usize,
) -> MaterializeStats {
    let (_, copy) = prepare(f);
    let stats = apply_manifest_diff(&copy, &f.base, &f.next, &f.blobs).unwrap();
    let cold = f.dir.path().join("fact-cold.sqlite");
    materialize_manifest_view_database(&cold, &f.blobs, &f.next).unwrap();
    assert_snapshot_parity(&snapshot(&cold), &snapshot(&copy));
    assert!(!stats.full_resolution, "{stats:?}");
    assert_eq!(stats.unattributed_callers, 0, "{stats:?}");
    assert_eq!(stats.dependent_files, expected_dependents, "{stats:?}");
    assert_eq!(stats.resolved_files, expected_resolved, "{stats:?}");
    println!("fact matrix: {stats:?}");
    stats
}

#[test]
fn version_only_package_edit_resolves_no_unchanged_callers() {
    let mut f = workspace_fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    config(
        &conn,
        &mut f.next,
        "package.json",
        r#"{"workspaces":["packages/*"],"version":"2"}"#,
    );
    let stats = check_transition(&f, 0, 0);
    assert_eq!(stats.rebuilt_surface_entries, 0);
    assert_eq!(stats.decoded_caller_blobs, 0);
}

#[test]
fn workspace_member_added_resolves_only_its_importers() {
    let mut f = workspace_fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    config(
        &conn,
        &mut f.base,
        "package.json",
        r#"{"workspaces":["packages/q"]}"#,
    );
    check_transition(&f, 2, 2);
}

#[test]
fn exports_change_resolves_only_package_importers() {
    let mut f = workspace_fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    config(
        &conn,
        &mut f.next,
        "packages/pkg/package.json",
        r#"{"name":"pkg","exports":"./two.ts"}"#,
    );
    check_transition(&f, 2, 2);
}

#[test]
fn tsconfig_paths_change_resolves_only_alias_importers() {
    let mut f = workspace_fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    config(
        &conn,
        &mut f.base,
        "tsconfig.json",
        r#"{"compilerOptions":{"paths":{"pkg":["packages/pkg/one.ts"]}}}"#,
    );
    config(
        &conn,
        &mut f.next,
        "tsconfig.json",
        r#"{"compilerOptions":{"paths":{"pkg":["packages/pkg/two.ts"]}}}"#,
    );
    check_transition(&f, 2, 2);
}

#[test]
fn cargo_workspace_member_added_resolves_its_dependents() {
    let mut f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let caller = "pub fn caller() { new_crate::target(); }";
    f.base = manifest(
        &conn,
        &[
            ("app/src/lib.rs", caller),
            ("unrelated/src/lib.rs", "pub fn other() {}"),
        ],
    );
    config(
        &conn,
        &mut f.base,
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\"]",
    );
    config(
        &conn,
        &mut f.base,
        "app/Cargo.toml",
        "[package]\nname = \"app\"",
    );
    f.next = manifest(
        &conn,
        &[
            ("app/src/lib.rs", caller),
            ("unrelated/src/lib.rs", "pub fn other() {}"),
            ("new/src/lib.rs", "pub fn target() {}"),
        ],
    );
    config(
        &conn,
        &mut f.next,
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"new\"]",
    );
    config(
        &conn,
        &mut f.next,
        "app/Cargo.toml",
        "[package]\nname = \"app\"",
    );
    config(
        &conn,
        &mut f.next,
        "new/Cargo.toml",
        "[package]\nname = \"new-crate\"",
    );
    check_transition(&f, 1, 2);
}

#[test]
fn rust_mod_move_resolves_declaring_crate_callers() {
    let mut f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let caller = "mod target; pub fn caller() { target::target(); }";
    f.base = manifest(
        &conn,
        &[
            ("src/lib.rs", caller),
            ("src/target.rs", "pub fn target() {}"),
            ("other/src/lib.rs", "pub fn other() {}"),
        ],
    );
    f.next = manifest(
        &conn,
        &[
            ("src/lib.rs", caller),
            ("src/target/mod.rs", "pub fn target() {}"),
            ("other/src/lib.rs", "pub fn other() {}"),
        ],
    );
    check_transition(&f, 1, 2);
}

#[test]
fn unattributed_binding_forces_counted_full_fallback() {
    let mut f = workspace_fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    config(
        &conn,
        &mut f.next,
        "package.json",
        r#"{"workspaces":["packages/*"],"version":"2"}"#,
    );
    let (_, copy) = prepare(&f);
    let db = Connection::open(&copy).unwrap();
    let mut bindings = load_bindings(&db).unwrap();
    bindings.get_mut("a.ts").unwrap().unattributed = true;
    db.execute(
        "UPDATE view_bindings SET payload=?1 WHERE file_path='a.ts'",
        [serde_json::to_string(&bindings["a.ts"]).unwrap()],
    )
    .unwrap();
    let stats = apply_manifest_diff(&copy, &f.base, &f.next, &f.blobs).unwrap();
    assert!(stats.full_resolution);
    assert_eq!(stats.unattributed_callers, 1);
    let cold = f.dir.path().join("fallback-cold.sqlite");
    materialize_manifest_view_database(&cold, &f.blobs, &f.next).unwrap();
    assert_snapshot_parity(&snapshot(&cold), &snapshot(&copy));
}

#[test]
fn fresh_manifest_cold_child() {
    let Some(dir) = std::env::var_os("AFT_FACT_COLD_CHILD") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let manifest =
        Manifest::from_json_bytes(&std::fs::read(dir.join("next.json")).unwrap()).unwrap();
    materialize_manifest_view_database(
        &dir.join("fresh.sqlite"),
        &dir.join("blobs.sqlite"),
        &manifest,
    )
    .unwrap();
}

#[test]
fn cold_workspace_after_prior_manifest_matches_fresh_process() {
    let mut f = workspace_fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let extra = manifest(
        &conn,
        &[("packages/new/one.ts", "export function target() {}")],
    );
    f.next = Manifest::new(
        f.next
            .entries()
            .chain(extra.entries())
            .map(|(path, entry)| (path.clone(), entry.clone())),
    )
    .unwrap();
    config(
        &conn,
        &mut f.next,
        "packages/pkg/package.json",
        r#"{"name":"retired","exports":"./one.ts"}"#,
    );
    config(
        &conn,
        &mut f.next,
        "packages/new/package.json",
        r#"{"name":"pkg","exports":"./one.ts"}"#,
    );
    std::fs::write(
        f.dir.path().join("next.json"),
        f.next.to_json_bytes().unwrap(),
    )
    .unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "views::materialization::tests::fact_tests::fresh_manifest_cold_child",
            "--nocapture",
        ])
        .env("AFT_FACT_COLD_CHILD", f.dir.path())
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    materialize_manifest_view_database(&f.dir.path().join("prior.sqlite"), &f.blobs, &f.base)
        .unwrap();
    materialize_manifest_view_database(&f.dir.path().join("after.sqlite"), &f.blobs, &f.next)
        .unwrap();
    assert_snapshot_parity(
        &snapshot(&f.dir.path().join("fresh.sqlite")),
        &snapshot(&f.dir.path().join("after.sqlite")),
    );
}

#[test]
fn workspace_member_name_change_invalidates_memo_hit_callers() {
    let mut f = workspace_fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    config(
        &conn,
        &mut f.next,
        "packages/pkg/package.json",
        r#"{"name":"retired","exports":"./one.ts"}"#,
    );
    check_transition(&f, 2, 2);
}

#[test]
fn workspace_discovery_uses_one_config_membership_dependency() {
    let f = workspace_fixture();
    let (base, _) = prepare(&f);
    let bindings = load_bindings(&Connection::open(base).unwrap()).unwrap();
    for caller in ["a.ts", "b.ts", "q.ts"] {
        let binding = &bindings[caller];
        assert!(binding
            .dependencies
            .contains(join::VIEW_CONFIG_MEMBERSHIP_DOMAIN));
        assert!(
            binding
                .dependencies
                .iter()
                .all(|path| !join::view_resolution_config(path.as_bytes())),
            "{caller} stored directory-wide config probes as individual dependency rows"
        );
    }
    assert!(
        !bindings["q.ts"]
            .consulted_facts
            .contains(&("packages/pkg/package.json".into(), "exports".into())),
        "name discovery must not subscribe to unrelated entry points"
    );
}
