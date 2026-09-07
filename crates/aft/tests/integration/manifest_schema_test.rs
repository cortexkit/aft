use aft::views::{
    probe_publication_closure, ArtifactPlane, ClosureRequirements, Manifest, ManifestEntry,
    ManifestEntryKind, PublicationClosure, RegularPlanes, RelPath, Result, SyntheticPlanes,
    ViewError, PATH_IDENTITY_VERSION,
};
use std::collections::BTreeSet;
use std::fs;
use tempfile::tempdir;

fn regular(callgraph: Option<&str>, resolution_input: bool) -> ManifestEntry {
    ManifestEntry::Regular {
        mode: 0o100644,
        planes: RegularPlanes {
            semantic: Some("semantic-key".to_string()),
            callgraph: callgraph.map(str::to_owned),
        },
        resolution_input,
    }
}

fn member<'a>(manifest: &'a Manifest, path: &[u8]) -> &'a ManifestEntry {
    manifest.get(&RelPath::new(path.to_vec()).unwrap()).unwrap()
}

#[test]
fn manifest_schema_carries_every_q3_member_and_keeps_lockfiles_out_of_resolution() {
    let fixture = tempdir().unwrap();
    let root = fixture.path();
    for relative in [
        ".gitignore",
        "nested/.gitignore",
        "tsconfig.json",
        "nested/tsconfig.build.json",
        "jsconfig.json",
        "package.json",
        "packages/app/package.json",
        "Cargo.toml",
        "crates/member/Cargo.toml",
        ".gitmodules",
        "Cargo.lock",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        ".git/info/exclude",
    ] {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "fixture").unwrap();
    }
    let global_ignore = fixture.path().join("global-ignore");
    fs::write(&global_ignore, "*.generated").unwrap();

    let mut entries = vec![
        (
            RelPath::synthetic("global-gitignore").unwrap(),
            ManifestEntry::Synthetic {
                name: "global-gitignore".to_string(),
                planes: SyntheticPlanes {
                    callgraph: "global-ignore-key".to_string(),
                },
            },
        ),
        (
            RelPath::synthetic(".git/info/exclude").unwrap(),
            ManifestEntry::Synthetic {
                name: ".git/info/exclude".to_string(),
                planes: SyntheticPlanes {
                    callgraph: "exclude-key".to_string(),
                },
            },
        ),
    ];
    for path in [
        ".gitignore",
        "nested/.gitignore",
        "tsconfig.json",
        "nested/tsconfig.build.json",
        "jsconfig.json",
        "package.json",
        "packages/app/package.json",
        "Cargo.toml",
        "crates/member/Cargo.toml",
        ".gitmodules",
    ] {
        entries.push((
            RelPath::new(path.as_bytes().to_vec()).unwrap(),
            regular(Some("config-key"), true),
        ));
    }
    for path in [
        "Cargo.lock",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
    ] {
        entries.push((
            RelPath::new(path.as_bytes().to_vec()).unwrap(),
            regular(None, false),
        ));
    }
    let manifest = Manifest::new(entries).unwrap();

    assert_eq!(manifest.path_identity_version, PATH_IDENTITY_VERSION);
    for path in [
        b".gitignore".as_slice(),
        b"nested/.gitignore",
        b"tsconfig.json",
        b"nested/tsconfig.build.json",
        b"jsconfig.json",
        b"package.json",
        b"packages/app/package.json",
        b"Cargo.toml",
        b"crates/member/Cargo.toml",
        b".gitmodules",
    ] {
        assert_eq!(member(&manifest, path).kind(), ManifestEntryKind::Regular);
    }
    for lockfile in [
        b"Cargo.lock".as_slice(),
        b"package-lock.json",
        b"pnpm-lock.yaml",
        b"yarn.lock",
    ] {
        assert!(matches!(
            member(&manifest, lockfile),
            ManifestEntry::Regular {
                resolution_input: false,
                ..
            }
        ));
    }
    for synthetic in ["global-gitignore", ".git/info/exclude"] {
        assert!(matches!(
            manifest.get(&RelPath::synthetic(synthetic).unwrap()),
            Some(ManifestEntry::Synthetic { .. })
        ));
    }

    let ordered_paths = manifest
        .entries()
        .map(|(path, _)| path.as_bytes().to_vec())
        .collect::<Vec<_>>();
    assert_eq!(ordered_paths[0], b"\0.git/info/exclude");
    assert_eq!(ordered_paths[1], b"\0global-gitignore");
}

#[test]
fn manifest_round_trips_non_utf8_paths_and_the_full_tagged_union() {
    let non_utf8 = RelPath::new(b"src/invalid-\xff.rs".to_vec()).unwrap();
    let manifest = Manifest::new([
        (non_utf8.clone(), regular(Some("callgraph-key"), true)),
        (
            RelPath::new(b"link".to_vec()).unwrap(),
            ManifestEntry::Symlink {
                target_bytes: b"target-\xff".as_slice().into(),
            },
        ),
        (
            RelPath::new(b"submodule".to_vec()).unwrap(),
            ManifestEntry::Gitlink {
                oid: "0123456789012345678901234567890123456789".to_string(),
            },
        ),
        (
            RelPath::synthetic("global-gitignore").unwrap(),
            ManifestEntry::Synthetic {
                name: "global-gitignore".to_string(),
                planes: SyntheticPlanes {
                    callgraph: "ignore-key".to_string(),
                },
            },
        ),
    ])
    .unwrap();

    let json = String::from_utf8(manifest.to_json_bytes().unwrap()).unwrap();
    assert!(json.contains("\"path_identity_version\":1"));
    assert!(json.contains("\"b64\":\"c3JjL2ludmFsaWQt/y5ycw==\""));
    assert!(json.contains("\"kind\":\"regular\""));
    assert!(json.contains("\"kind\":\"symlink\""));
    assert!(json.contains("\"kind\":\"gitlink\""));
    assert!(json.contains("\"kind\":\"synthetic\""));
    assert_eq!(
        Manifest::from_json_bytes(json.as_bytes()).unwrap(),
        manifest
    );
    assert_eq!(
        member(&manifest, non_utf8.as_bytes()).kind(),
        ManifestEntryKind::Regular
    );
}

struct Closure {
    missing: Option<(ArtifactPlane, String)>,
    trigram: bool,
    aliases: BTreeSet<String>,
}

impl PublicationClosure for Closure {
    fn contains_blob(&self, plane: ArtifactPlane, key: &str) -> Result<bool> {
        Ok(self.missing.as_ref() != Some(&(plane, key.to_string())))
    }

    fn trigram_is_present(&self) -> Result<bool> {
        Ok(self.trigram)
    }

    fn contains_alias(&self, git_oid: &str) -> Result<bool> {
        Ok(self.aliases.contains(git_oid))
    }
}

#[test]
fn closure_probe_checks_every_non_null_key_trigram_and_alias() {
    let manifest = Manifest::new([(
        RelPath::new(b"src/main.rs".to_vec()).unwrap(),
        regular(Some("callgraph-key"), false),
    )])
    .unwrap();
    let requirements = ClosureRequirements {
        referenced_aliases: BTreeSet::from(["git-oid".to_string()]),
    };
    let closure = Closure {
        missing: None,
        trigram: true,
        aliases: BTreeSet::from(["git-oid".to_string()]),
    };
    probe_publication_closure(&manifest, &requirements, &closure).unwrap();

    let missing = Closure {
        missing: Some((ArtifactPlane::Callgraph, "callgraph-key".to_string())),
        trigram: true,
        aliases: BTreeSet::from(["git-oid".to_string()]),
    };
    assert!(matches!(
        probe_publication_closure(&manifest, &requirements, &missing),
        Err(ViewError::MissingBlob {
            plane: ArtifactPlane::Callgraph,
            ..
        })
    ));

    let missing_trigram = Closure {
        missing: None,
        trigram: false,
        aliases: BTreeSet::from(["git-oid".to_string()]),
    };
    assert!(matches!(
        probe_publication_closure(&manifest, &requirements, &missing_trigram),
        Err(ViewError::MissingTrigram)
    ));

    let missing_alias = Closure {
        missing: None,
        trigram: true,
        aliases: BTreeSet::new(),
    };
    assert!(matches!(
        probe_publication_closure(&manifest, &requirements, &missing_alias),
        Err(ViewError::MissingAlias(oid)) if oid == "git-oid"
    ));
}
