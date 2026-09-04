use super::super::disk_facts::DiskFacts;
use super::super::facts::{path_bytes, DirEntry, EntryKind};
use super::super::{
    build_file_extract, resolve_ref, DbFileIndex, ProjectIndex, WorkspaceCratePrefixCache,
};
use super::*;
use crate::views::{ByteString, RegularPlanes};

#[derive(Default)]
struct MemoryBlobs(BTreeMap<String, Vec<u8>>);
impl ManifestBlobReader for MemoryBlobs {
    fn read_callgraph_blob(&self, key: &str) -> Result<Option<Vec<u8>>, ManifestJoinError> {
        Ok(self.0.get(key).cloned())
    }
}

fn regular(key: &str, resolution_input: bool) -> ManifestEntry {
    ManifestEntry::Regular {
        mode: 0o100644,
        planes: RegularPlanes {
            semantic: None,
            callgraph: Some(key.to_string()),
        },
        resolution_input,
    }
}

fn fixture() -> (tempfile::TempDir, Manifest, MemoryBlobs) {
    let root = tempfile::tempdir().unwrap();
    let inputs = [
        ("package.json", r#"{"private":true,"workspaces":["packages/*"]}"#, "config"),
        ("tsconfig.json", r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib/*":["lib/*"]}}}"#, "config"),
        ("packages/math/package.json", r#"{"name":"@fixture/math","exports":{".":"./src/index.ts"}}"#, "config"),
        ("app/main.ts", "import { alias } from '@lib/alias';\nimport { packageFn } from '@fixture/math';\nimport { relative } from './relative';\nimport { indexed } from './directory';\nexport function run() { alias(); packageFn(); relative(); indexed(); }\n", "typescript"),
        ("app/relative.ts", "export function relative() {}\n", "typescript"),
        ("app/directory/index.ts", "export function indexed() {}\n", "typescript"),
        ("lib/alias.ts", "export function alias() {}\n", "typescript"),
        ("packages/math/src/index.ts", "export function packageFn() {}\n", "typescript"),
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n", "config"),
        ("crates/app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\n", "config"),
        ("crates/helper/Cargo.toml", "[package]\nname = \"helper-crate\"\nversion = \"0.1.0\"\n", "config"),
        ("crates/app/src/lib.rs", "mod worker;\n#[path = \"alternate.rs\"] mod named;\npub fn run() { worker::work(); named::other(); helper_crate::help(); }\n", "rust"),
        ("crates/app/src/worker.rs", "pub fn work() {}\n", "rust"),
        ("crates/app/src/alternate.rs", "pub fn other() {}\n", "rust"),
        ("crates/helper/src/lib.rs", "pub fn help() {}\n", "rust"),
    ];
    let mut blobs = MemoryBlobs::default();
    let mut entries = Vec::new();
    for (path, source, language) in inputs {
        let file = root.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, source).unwrap();
        let blob = if language == "config" {
            CallgraphBlob::config(source, "join-v1")
        } else {
            CallgraphBlob::extract(source, language, "join-v1").unwrap()
        };
        let bytes = blob.to_bytes().unwrap();
        let key = blake3::hash(&bytes).to_hex().to_string();
        blobs.0.insert(key.clone(), bytes);
        entries.push((
            RelPath::new(path.as_bytes().to_vec()).unwrap(),
            regular(&key, language == "config"),
        ));
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = b"app/non-utf8-\xff.ts";
        let source = "export function unbound() { absent(); }";
        std::fs::write(root.path().join(std::ffi::OsStr::from_bytes(path)), source).unwrap();
        let bytes = CallgraphBlob::extract(source, "typescript", "join-v1")
            .unwrap()
            .to_bytes()
            .unwrap();
        let key = blake3::hash(&bytes).to_hex().to_string();
        blobs.0.insert(key.clone(), bytes);
        entries.push((RelPath::new(path.to_vec()).unwrap(), regular(&key, false)));
    }
    (root, Manifest::new(entries).unwrap(), blobs)
}

/// Independently extract the checkout and run its normal index through resolve_ref.
fn disk_rows(root: &Path, manifest: &Manifest, blobs: &MemoryBlobs) -> JoinResult {
    let disk = Rc::new(DiskFacts::new(root));
    let paths = FactPaths {
        root,
        facts: disk.as_ref(),
    };
    let mut extracts = HashMap::new();
    let mut work = Vec::new();
    let mut unbound_non_utf8_paths = Vec::new();
    for (path, entry) in manifest.entries() {
        let ManifestEntry::Regular { planes, .. } = entry else {
            continue;
        };
        let key = planes.callgraph.as_ref().unwrap();
        let CallgraphBlob::Parse(blob) = CallgraphBlob::from_bytes(&blobs.0[key]).unwrap() else {
            continue;
        };
        let Ok(rel) = std::str::from_utf8(path.as_bytes()) else {
            unbound_non_utf8_paths.push(path.as_bytes().to_vec());
            continue;
        };
        let extract = build_file_extract(root, &root.join(rel)).unwrap();
        for raw in &blob.refs {
            let kind = match raw.kind {
                BlobRefKind::Call => "call",
                BlobRefKind::ValueRef => "value_ref",
                BlobRefKind::Import => "import",
                BlobRefKind::Module => "module",
                BlobRefKind::Reexport => "reexport",
                BlobRefKind::ExportAlias => "export_alias",
            };
            let bound = extract
                .raw_refs
                .iter()
                .find(|bound| {
                    bound.kind == kind
                        && bound.byte_start == raw.byte_start
                        && bound.byte_end == raw.byte_end
                })
                .unwrap_or_else(|| {
                    panic!(
                        "disk extract missing {rel} {kind} {}..{}",
                        raw.byte_start, raw.byte_end
                    )
                });
            work.push((
                CallerRefKey {
                    caller_blob_key: key.clone(),
                    ref_ordinal: raw.ordinal,
                    caller_path: path.as_bytes().to_vec(),
                },
                (raw.kind, bound.clone()),
            ));
        }
        extracts.insert(rel.to_string(), extract);
    }
    let files = extracts
        .iter()
        .map(|(path, extract)| {
            (
                path.clone(),
                DbFileIndex::from_extract(root, extract, &paths),
            )
        })
        .collect();
    let caller_data = extracts
        .iter()
        .map(|(path, extract)| (path.clone(), &extract.data))
        .collect();
    let mut index = ProjectIndex::from_parts(
        root,
        files,
        caller_data,
        WorkspaceCratePrefixCache::default(),
        disk,
    );
    index.unbound_non_utf8_paths = unbound_non_utf8_paths;
    let mut result = JoinResult {
        rows: BTreeSet::new(),
        resolution_order: Vec::new(),
        unbound_non_utf8_paths: index.unbound_non_utf8_paths.clone(),
    };
    work.sort_by(|a, b| (&a.0, a.1 .0).cmp(&(&b.0, b.1 .0)));
    for (key, (kind, raw)) in work {
        let resolved = resolve_ref(raw, &index).unwrap();
        result.rows.insert(DerivedRow {
            caller_blob_key: key.caller_blob_key.clone(),
            ref_ordinal: key.ref_ordinal,
            caller_path: key.caller_path.clone(),
            kind,
            status: if resolved.target_file.is_some() {
                ResolutionStatus::Resolved
            } else {
                ResolutionStatus::Unresolved
            },
            target_path: resolved.target_file.map(String::into_bytes),
            target_symbol: resolved.target_symbol,
        });
        result.resolution_order.push(key);
    }
    result
}

#[test]
fn disk_and_manifest_resolve_ref_rows_are_identical() {
    let (root, manifest, blobs) = fixture();
    let root = std::fs::canonicalize(root.path()).unwrap();
    let disk = disk_rows(&root, &manifest, &blobs);
    let joined = JoinResult::from_manifest(&manifest, &blobs).unwrap();
    let expected = [
        "lib/alias.ts",
        "packages/math/src/index.ts",
        "app/relative.ts",
        "app/directory/index.ts",
        "crates/app/src/worker.rs",
        "crates/app/src/alternate.rs",
        "crates/helper/src/lib.rs",
    ];
    for path in expected {
        assert!(
            disk.rows
                .iter()
                .any(|row| row.target_path.as_deref() == Some(path.as_bytes())),
            "fixture must resolve {path}"
        );
    }
    assert_eq!(joined.unbound_non_utf8_paths, disk.unbound_non_utf8_paths);
    #[cfg(target_os = "linux")]
    assert_eq!(
        joined.unbound_non_utf8_paths,
        vec![b"app/non-utf8-\xff.ts".to_vec()]
    );
    assert_eq!(joined.rows, disk.rows);
    assert_eq!(
        joined.canonical_serialization(),
        disk.canonical_serialization()
    );
}

#[test]
fn shuffled_manifest_and_blob_input_preserve_logical_rows() {
    let (_root, manifest, blobs) = fixture();
    let mut entries = manifest
        .entries()
        .map(|(p, e)| (p.clone(), e.clone()))
        .collect::<Vec<_>>();
    entries.reverse();
    let shuffled = Manifest::new(entries).unwrap();
    let shuffled_blobs = MemoryBlobs(
        blobs
            .0
            .iter()
            .rev()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    let result = JoinResult::from_manifest(&manifest, &blobs).unwrap();
    let again = JoinResult::from_manifest(&shuffled, &shuffled_blobs).unwrap();
    assert_eq!(result, again);
    assert_eq!(
        result.canonical_serialization(),
        again.canonical_serialization()
    );
    assert!(result
        .resolution_order
        .windows(2)
        .all(|pair| pair[0] <= pair[1]));
    for bytes in blobs.0.values() {
        assert_eq!(
            *bytes,
            CallgraphBlob::from_bytes(bytes)
                .unwrap()
                .to_bytes()
                .unwrap()
        );
    }
}

fn replace_member(
    manifest: &Manifest,
    blobs: &mut MemoryBlobs,
    path: &str,
    blob: CallgraphBlob,
    config: bool,
) -> Manifest {
    let bytes = blob.to_bytes().unwrap();
    let key = blake3::hash(&bytes).to_hex().to_string();
    blobs.0.insert(key.clone(), bytes);
    Manifest::new(
        manifest
            .entries()
            .filter(|(p, _)| p.as_bytes() != path.as_bytes())
            .map(|(p, e)| (p.clone(), e.clone()))
            .chain(std::iter::once((
                RelPath::new(path.as_bytes().to_vec()).unwrap(),
                regular(&key, config),
            ))),
    )
    .unwrap()
}

#[test]
fn invalidation_tracks_own_refs_previous_targets_configs_and_lockfiles() {
    let (_root, manifest, mut blobs) = fixture();
    let before = JoinResult::from_manifest(&manifest, &blobs).unwrap();
    let changed = replace_member(
        &manifest,
        &mut blobs,
        "app/relative.ts",
        CallgraphBlob::extract(
            "export function relative() { absent(); }\n",
            "typescript",
            "join-v1",
        )
        .unwrap(),
        false,
    );
    let update = before.update(&manifest, &changed, &blobs).unwrap();
    let expected = update
        .result
        .rows
        .iter()
        .filter(|row| {
            row.caller_path == b"app/relative.ts"
                || before.rows.iter().any(|previous| {
                    previous.ref_key() == row.ref_key()
                        && previous.target_path.as_deref() == Some(b"app/relative.ts")
                })
        })
        .map(DerivedRow::ref_key)
        .collect::<BTreeSet<_>>();
    assert!(!expected.is_empty());
    assert_eq!(update.re_resolved, expected);
    assert!(!update.full_re_resolve);
    assert_eq!(
        update.result,
        JoinResult::from_manifest(&changed, &blobs).unwrap()
    );
    let removed = Manifest::new(
        manifest
            .entries()
            .filter(|(p, _)| p.as_bytes() != b"app/relative.ts")
            .map(|(p, e)| (p.clone(), e.clone())),
    )
    .unwrap();
    let update = before.update(&manifest, &removed, &blobs).unwrap();
    assert_eq!(
        update.re_resolved,
        before
            .rows
            .iter()
            .filter(|row| row.target_path.as_deref() == Some(b"app/relative.ts"))
            .map(DerivedRow::ref_key)
            .collect()
    );
    assert_eq!(
        update.result,
        JoinResult::from_manifest(&removed, &blobs).unwrap()
    );
    let config = replace_member(
        &manifest,
        &mut blobs,
        "tsconfig.json",
        CallgraphBlob::config(br#"{"compilerOptions":{"paths":{}}}"#, "join-v1"),
        true,
    );
    let update = before.update(&manifest, &config, &blobs).unwrap();
    assert!(update.full_re_resolve);
    assert_eq!(
        update.re_resolved.len(),
        update.result.resolution_order.len()
    );
    let locked = replace_member(
        &manifest,
        &mut blobs,
        "Cargo.lock",
        CallgraphBlob::config("version = 4", "join-v1"),
        false,
    );
    let update = before.update(&manifest, &locked, &blobs).unwrap();
    assert!(!update.full_re_resolve);
    assert!(update.re_resolved.is_empty());
    assert_eq!(before, update.result);
}

#[test]
fn configuration_resolution_reads_blobs_not_checkout() {
    let (root, manifest, blobs) = fixture();
    let expected = JoinResult::from_manifest(&manifest, &blobs).unwrap();
    std::fs::write(root.path().join("tsconfig.json"), "not json").unwrap();
    std::fs::write(root.path().join("packages/math/package.json"), "not json").unwrap();
    std::fs::write(root.path().join("crates/helper/Cargo.toml"), "not toml").unwrap();
    root.close().unwrap();
    assert_eq!(
        expected,
        JoinResult::from_manifest(&manifest, &blobs).unwrap()
    );
}

#[test]
fn disk_facts_match_direct_probes_and_directory_walk() {
    let (root, _, _) = fixture();
    let root = std::fs::canonicalize(root.path()).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("app", root.join("linked")).unwrap();
    let disk = DiskFacts::new(&root);
    for rel in [
        "",
        "app",
        "app/main.ts",
        "absent.ts",
        "linked",
        "linked/main.ts",
    ] {
        let path = root.join(rel);
        assert_eq!(disk.is_file(rel.as_bytes()), path.is_file(), "{rel}");
        assert_eq!(disk.is_dir(rel.as_bytes()), path.is_dir(), "{rel}");
        let canonical = super::super::canonicalize_path(&path);
        let canonical = super::super::relative_path(&root, &canonical);
        assert_eq!(
            disk.canonical(rel.as_bytes()),
            Some(canonical.into_bytes()),
            "{rel}"
        );
    }
    let boundary = crate::walk_boundary::DeviceBoundary::for_root(&root).unwrap();
    let mut expected = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let ty = entry.file_type().unwrap();
            let kind = if ty.is_symlink() {
                EntryKind::Symlink
            } else if ty.is_dir() {
                assert!(boundary.should_descend(&entry.path()).unwrap());
                EntryKind::Directory
            } else {
                EntryKind::Regular
            };
            DirEntry {
                name: path_bytes(Path::new(&entry.file_name())),
                kind,
            }
        })
        .collect::<Vec<_>>();
    let mut actual = disk.list_dir(b"");
    expected.sort_by(|a, b| a.name.cmp(&b.name));
    actual.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(actual, expected);
    #[cfg(unix)]
    assert_eq!(disk.symlink_target(b"linked"), Some(b"app".as_slice()));
}

#[test]
fn manifest_facts_use_entry_identity_for_symlinks_and_gitlinks() {
    let (_root, manifest, blobs) = fixture();
    let additions = [
        (
            "linked",
            ManifestEntry::Symlink {
                target_bytes: ByteString::new(b"app".to_vec()),
            },
        ),
        (
            "cycle",
            ManifestEntry::Symlink {
                target_bytes: ByteString::new(b"cycle".to_vec()),
            },
        ),
        (
            "external",
            ManifestEntry::Gitlink {
                oid: "a".repeat(40),
            },
        ),
    ];
    let manifest = Manifest::new(
        manifest
            .entries()
            .map(|(p, e)| (p.clone(), e.clone()))
            .chain(
                additions
                    .into_iter()
                    .map(|(p, e)| (RelPath::new(p.as_bytes().to_vec()).unwrap(), e)),
            ),
    )
    .unwrap();
    let loaded = manifest_payloads(&manifest, &blobs).unwrap();
    let reader = |key: &BlobKey| loaded.get(key).cloned();
    let facts = ManifestFacts {
        manifest: &manifest,
        blobs: &reader,
    };
    assert_eq!(
        facts.canonical(b"linked/main.ts"),
        Some(b"app/main.ts".to_vec())
    );
    assert_eq!(facts.canonical(b"cycle"), None);
    assert_eq!(facts.canonical(b"external"), Some(b"external".to_vec()));
    assert!(!facts.is_file(b"external"));
    assert!(!facts.is_dir(b"external"));
    assert_eq!(facts.canonical(b"../app/main.ts"), None);
    assert!(facts
        .list_dir(b"")
        .iter()
        .any(|entry| entry.name == b"linked" && entry.kind == EntryKind::Symlink));
}

#[test]
fn manifest_facts_source_has_no_filesystem_escape() {
    let source = include_str!("../../src/callgraph_store/facts.rs");
    let mut parser = Parser::new();
    parser.set_language(&grammar_for(LangId::Rust)).unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(!tree.root_node().has_error());
    let mut all_tokens = Vec::new();
    let mut pending = vec![tree.root_node()];
    while let Some(node) = pending.pop() {
        if matches!(
            node.kind(),
            "line_comment" | "block_comment" | "string_literal"
        ) {
            continue;
        }
        if node.child_count() == 0 {
            all_tokens.push(source[node.byte_range()].to_string());
        } else {
            pending.extend(
                node.children(&mut node.walk())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev(),
            );
        }
    }
    let all_tokens = all_tokens.join(" ");
    for banned in [
        "std :: fs",
        "fs ::",
        "File ::",
        "read_dir",
        "metadata (",
        "canonicalize",
        ". exists (",
    ] {
        assert!(
            !all_tokens.contains(banned),
            "facts module filesystem escape: {banned}"
        );
    }
    let mut stack = vec![tree.root_node()];
    let mut saw_impl = false;
    while let Some(node) = stack.pop() {
        if node.kind() == "impl_item" {
            let text = &source[node.byte_range()];
            if text.starts_with("impl ProjectFacts for ManifestFacts") {
                saw_impl = true;
                let mut leaves = Vec::new();
                let mut pending = vec![node];
                while let Some(child) = pending.pop() {
                    if matches!(
                        child.kind(),
                        "line_comment" | "block_comment" | "string_literal"
                    ) {
                        continue;
                    }
                    if child.child_count() == 0 {
                        leaves.push(source[child.byte_range()].to_string());
                    } else {
                        pending.extend(
                            child
                                .children(&mut child.walk())
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev(),
                        );
                    }
                }
                let tokens = leaves.join(" ");
                for banned in [
                    "std :: fs",
                    "fs ::",
                    "File ::",
                    "read_dir",
                    "metadata (",
                    "canonicalize",
                    ". exists (",
                ] {
                    assert!(
                        !tokens.contains(banned),
                        "manifest facts filesystem escape: {banned}"
                    );
                }
                for banned in [". is_file (", ". is_dir ("] {
                    // Membership calls in the impl must target the trait receiver,
                    // not an ambient Path value; canonical uses self.is_dir.
                    let without_self = tokens.replace(&format!("self {banned}"), "");
                    assert!(
                        !without_self.contains(banned),
                        "manifest facts Path probe: {banned}"
                    );
                }
            }
        }
        stack.extend(node.children(&mut node.walk()));
    }
    assert!(
        saw_impl,
        "scan must visit impl ProjectFacts for ManifestFacts"
    );
}
