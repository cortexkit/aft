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
