//! Compare logical tables and bound dispatch answers across independent roots.
//! Legacy name guesses are deliberately not a reference: both roots independently
//! extract immutable hints and rebuild a view.
use std::collections::BTreeMap;
use std::path::Path;

use crate::callgraph_store::join::{CallgraphBlob, ManifestBlobReader, ManifestJoinError};
use crate::callgraph_store::ReadonlyCallGraphStore;
use crate::views::materialization::parity::logical_snapshot;
use crate::views::{Manifest, ManifestEntry, RegularPlanes, RelPath, ViewStore};

struct Blobs(BTreeMap<String, Vec<u8>>);
impl ManifestBlobReader for Blobs {
    fn read_callgraph_blob(&self, key: &str) -> Result<Option<Vec<u8>>, ManifestJoinError> {
        Ok(self.0.get(key).cloned())
    }
}
fn frozen(root: &Path) -> (Manifest, Blobs) {
    let source = std::fs::read_to_string(root.join("fixture.ts")).unwrap();
    let payload =
        CallgraphBlob::extract(&source, "typescript", crate::callgraph_extractor::PRODUCER)
            .unwrap()
            .to_bytes()
            .unwrap();
    let key = blake3::hash(&payload).to_hex().to_string();
    let manifest = Manifest::new([(
        RelPath::new(b"fixture.ts").unwrap(),
        ManifestEntry::Regular {
            mode: 0o100644,
            planes: RegularPlanes {
                callgraph: Some(key.clone()),
                semantic: None,
            },
            resolution_input: false,
        },
    )])
    .unwrap();
    (manifest, Blobs(BTreeMap::from([(key, payload)])))
}

#[test]
fn ruled_rows_are_root_independent_with_unreadable_checkouts() {
    let temporary = tempfile::tempdir().unwrap();
    let source = "interface I { m(): void; }\nclass A implements I { m() {} }\nclass B implements I { m() {} }\nexport function caller(x: I) { x.m(); }\nfunction unknown(x) { x.m(); }";
    let mut built = Vec::new();
    for (index, config) in [
        "{\"compilerOptions\":{\"baseUrl\":\"bad\"}}",
        "{\"compilerOptions\":{\"paths\":{\"*\":[\"wrong\"]}}}",
    ]
    .iter()
    .enumerate()
    {
        let parent = temporary.path().join(format!("parent-{index}"));
        let root = parent.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(parent.join("tsconfig.json"), config).unwrap();
        std::fs::write(root.join("nonmember-config.json"), config).unwrap();
        std::fs::write(root.join("fixture.ts"), source).unwrap();
        let (manifest, blobs) = frozen(&root);
        // Moving the checkout away makes every original source path unreadable,
        // including when the test runs as a user that bypasses mode permissions.
        std::fs::rename(&root, parent.join("unreadable-original-root")).unwrap();
        assert!(!root.exists());
        let views = ViewStore::open_dir(temporary.path().join(format!("view-{index}"))).unwrap();
        let database = views.derived_path("root-independent").unwrap();
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        super::materialization::materialize_from_blob_reader(&database, &manifest, &blobs).unwrap();
        let rows = logical_snapshot(&database);
        let logical_text = format!("{rows:?}");
        assert!(
            !logical_text.contains("TEXT /")
                && !logical_text.contains(temporary.path().to_str().unwrap()),
            "derived rows contain an absolute checkout/storage root"
        );
        let reader = ReadonlyCallGraphStore::open_pinned_derived(
            root.clone(),
            "family".into(),
            views.view_dir().to_path_buf(),
            "root-independent",
        )
        .unwrap();
        let callers = reader
            .callers_of(Path::new("fixture.ts"), "A::m", 1)
            .unwrap();
        // `caller(x: I)` reaches A::m through typed interface dispatch;
        // `unknown(x)` has an untyped receiver, so it is a name-only candidate.
        let by_symbol = callers
            .callers
            .iter()
            .map(|c| (c.caller.symbol.as_str(), c.provenance.as_ref()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            by_symbol,
            BTreeMap::from([("caller", "dispatch"), ("unknown", "name_match")])
        );
        let bound = callers
            .callers
            .into_iter()
            .map(|c| (c.caller.file, c.caller.symbol, c.line, c.provenance))
            .collect::<Vec<_>>();
        built.push((rows, bound));
    }
    assert_eq!(
        built[0], built[1],
        "independent cold builds and bound answers differ"
    );
}
