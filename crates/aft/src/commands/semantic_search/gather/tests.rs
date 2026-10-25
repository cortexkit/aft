use super::*;
use crate::callgraph_store::CallGraphStore;
use crate::config::Config;
use crate::context::AppContext;
use crate::parser::TreeSitterProvider;
use std::path::{Path, PathBuf};

const SOURCE: &str = "/// Load application settings.\npub fn load_config() {\n    parse_config();\n}\nfn parse_config() {\n    read_config();\n    external_library();\n}\nfn read_config() { parse_config(); }\n#[cfg(test)]\nfn test_caller() { parse_config(); }\n";

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    dir: PathBuf,
    file: PathBuf,
    ctx: AppContext,
    store: CallGraphStore,
}

impl Fixture {
    fn new(source: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &key, false);
        let dir = temp.path().join("storage/callgraph").join(key);
        let file = root.join("main.rs");
        std::fs::write(&file, source).unwrap();
        let (store, _) = CallGraphStore::cold_build_with_lease(
            dir.clone(),
            root.clone(),
            std::slice::from_ref(&file),
        )
        .unwrap();
        let mut config = Config::default();
        config.project_root = Some(root.clone());
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
        Self {
            _temp: temp,
            root,
            dir,
            file,
            ctx,
            store,
        }
    }

    fn seeds(&self, line: u32) -> Vec<GatherNode> {
        resolve_seeds(&self.ctx, &self.root, &[(self.file.clone(), line)], true)
    }
}

#[test]
fn gather_graph_containment_is_innermost_and_one_based() {
    let f =
        Fixture::new("fn outer() {\n    fn inner() {\n        work();\n    }\n    inner();\n}\n");
    assert_eq!(
        resolve_containing_symbol(&f.file, 3, &f.ctx),
        Some(("inner".into(), 2))
    );
    assert_eq!(
        resolve_containing_symbol(&f.file, 5, &f.ctx),
        Some(("outer".into(), 1))
    );
    assert_eq!(resolve_containing_symbol(&f.file, 0, &f.ctx), None);
}

#[test]
fn gather_graph_seeds_normalize_and_limit_before_dedup() {
    let f = Fixture::new(SOURCE);
    let hits = vec![
        (f.file.clone(), 3),
        (PathBuf::from("./main.rs"), 2),
        (f.file.clone(), 3),
        (f.file.clone(), 3),
        (f.file.clone(), 3),
        (f.file.clone(), 6),
    ];
    let seeds = resolve_seeds(&f.ctx, &f.root, &hits, false);
    assert_eq!(seeds.len(), 1);
    assert_eq!(seeds[0].path, Path::new("main.rs"));
    assert_eq!(seeds[0].id, "main.rs:2:load_config");
    assert_eq!(seeds[0].seed_ordinal, 0);
    assert_eq!(seeds[0].line, 2);
    assert_eq!(seeds[0].docs.len(), 1);
    assert!(seeds[0].signature.contains("load_config"));
    assert_eq!(seeds[0].kind, "function");
    assert!(seeds[0].body.contains("parse_config();"));
    assert!(!seeds[0].body.contains("fn read_config"));
}

#[test]
fn gather_graph_non_code_and_outside_hits_are_rejected() {
    let f = Fixture::new(SOURCE);
    std::fs::write(f.root.join("notes.md"), "# Heading\ntext\n").unwrap();
    let hits = [
        (PathBuf::from("notes.md"), 1),
        (PathBuf::from("../escape.rs"), 1),
        (f.file.clone(), 0),
        (f.file.clone(), 1),
        (f.file.clone(), 6),
    ];
    let seeds = resolve_seeds(&f.ctx, &f.root, &hits, false);
    assert_eq!(seeds.len(), 2);
    assert_eq!(
        seeds[0].name, "load_config",
        "doc hits retain full parser containment"
    );
    assert_eq!(seeds[0].seed_ordinal, 3);
    assert_eq!(seeds[1].name, "parse_config");
    assert_eq!(seeds[1].seed_ordinal, 4);
}

#[test]
fn gather_graph_bidirectional_edges_use_definitions_and_terminate_cycles() {
    let f = Fixture::new(SOURCE);
    let seeds = f.seeds(6);
    let snapshot = f.store.read_snapshot().unwrap();
    let nodes = collect_callgraph_neighbors(&f.ctx, &snapshot, &seeds, false).unwrap();
    assert_eq!(
        nodes.len(),
        2,
        "same neighbor reached by both directions is deduplicated"
    );
    let load = nodes.iter().find(|n| n.name == "load_config").unwrap();
    assert_eq!(
        load.line, 2,
        "caller sites must resolve to definition lines"
    );
    assert_eq!(load.edge, "caller-of-main.rs:5:parse_config");
    assert!(nodes
        .iter()
        .all(|n| n.hop_distance == 1 && n.seed_ordinal == 0));
    assert!(!nodes
        .iter()
        .any(|n| n.name == "external_library" || n.name == "parse_config"));
}

#[test]
fn gather_graph_include_tests_applies_in_both_directions() {
    let f = Fixture::new("fn production() { test_leaf(); }\n#[cfg(test)]\nfn test_leaf() {}\n#[cfg(test)]\nfn test_caller() { production(); }\n");
    let seeds = f.seeds(1);
    let snapshot = f.store.read_snapshot().unwrap();
    assert!(
        collect_callgraph_neighbors(&f.ctx, &snapshot, &seeds, false)
            .unwrap()
            .is_empty()
    );
    let included = collect_callgraph_neighbors(&f.ctx, &snapshot, &seeds, true).unwrap();
    assert_eq!(included.len(), 2);
    assert!(included.iter().any(|n| n.name == "test_leaf"));
    assert!(included.iter().any(|n| n.name == "test_caller"));
}

#[test]
fn gather_graph_duplicate_names_keep_distinct_ids() {
    let f = Fixture::new("fn same() {}\n");
    let other = f.root.join("other.rs");
    std::fs::write(&other, "fn same() {}\n").unwrap();
    let nodes = resolve_seeds(&f.ctx, &f.root, &[(f.file.clone(), 1), (other, 1)], false);
    assert_eq!(nodes.len(), 2);
    assert_ne!(nodes[0].id, nodes[1].id);
    assert_eq!(nodes[1].seed_ordinal, 1);
}

#[test]
fn gather_graph_body_has_utf8_byte_and_doc_bounds() {
    let f = Fixture::new(&format!(
        "/// One.\n/// Two.\n/// Three.\n/// Four.\nfn large() {{\n    let text = \"{}\";\n}}\n",
        "é".repeat(6000)
    ));
    let nodes = f.seeds(5);
    assert_eq!(nodes[0].docs.len(), 3);
    assert!(nodes[0].body.len() <= DEFAULT_BODY_BYTE_BUDGET);
    assert!(nodes[0].body.contains("fn large"));
}

#[test]
fn gather_graph_snapshot_pins_revision_and_cache_replaces_old_entries() {
    let f = Fixture::new("fn entry() { old_leaf(); }\nfn old_leaf() {}\nfn new_leaf() {}\n");
    let seeds = f.seeds(1);
    let snapshot = f.store.read_snapshot().unwrap();
    let before = snapshot.projection_write_revision().unwrap();
    assert_eq!(
        collect_callgraph_neighbors(&f.ctx, &snapshot, &seeds, false).unwrap()[0].name,
        "old_leaf"
    );
    std::fs::write(
        &f.file,
        "fn entry() { new_leaf(); }\nfn old_leaf() {}\nfn new_leaf() {}\n",
    )
    .unwrap();
    f.store
        .refresh_files(std::slice::from_ref(&f.file))
        .unwrap();
    assert_eq!(snapshot.projection_write_revision().unwrap(), before);
    assert_eq!(
        collect_callgraph_neighbors(&f.ctx, &snapshot, &seeds, false).unwrap()[0].name,
        "old_leaf"
    );
    let next = f.store.read_snapshot().unwrap();
    assert_ne!(next.projection_write_revision().unwrap(), before);
    assert_eq!(
        collect_callgraph_neighbors(&f.ctx, &next, &seeds, false).unwrap()[0].name,
        "new_leaf"
    );
    let cache = f.ctx.gather_runtime().neighbors.lock().unwrap();
    assert!(cache
        .entries
        .keys()
        .all(|key| key.identity.revision == next.projection_write_revision().unwrap().unwrap()));
}

#[test]
fn gather_graph_new_generation_does_not_change_pinned_walk() {
    let f = Fixture::new("fn entry() { old_leaf(); }\nfn old_leaf() {}\nfn new_leaf() {}\n");
    let seeds = f.seeds(1);
    let owner = CallGraphStore::open_readonly(f.dir.clone(), f.root.clone())
        .unwrap()
        .unwrap();
    let snapshot = owner.read_snapshot().unwrap();
    let generation = snapshot.projection_generation().unwrap().to_owned();
    snapshot.projection_write_revision().unwrap();
    std::fs::write(
        &f.file,
        "fn entry() { new_leaf(); }\nfn old_leaf() {}\nfn new_leaf() {}\n",
    )
    .unwrap();
    drop(f.store);
    let (published, _) = CallGraphStore::force_cold_build_with_lease_chunked(
        f.dir.clone(),
        f.root.clone(),
        std::slice::from_ref(&f.file),
        0,
    )
    .unwrap();
    assert_ne!(published.projection_generation(), Some(generation.as_str()));
    assert_eq!(
        collect_callgraph_neighbors(&f.ctx, &snapshot, &seeds, false).unwrap()[0].name,
        "old_leaf"
    );
    let next = published.read_snapshot().unwrap();
    assert_eq!(
        collect_callgraph_neighbors(&f.ctx, &next, &seeds, false).unwrap()[0].name,
        "new_leaf"
    );
    let cache = f.ctx.gather_runtime().neighbors.lock().unwrap();
    assert!(cache
        .entries
        .keys()
        .all(|key| key.identity.generation == next.projection_generation().unwrap()));
}

#[test]
fn gather_graph_cache_separates_test_file_neighbors() {
    let f = Fixture::new("fn unused() {}\n");
    let entry = f.root.join("entry.ts");
    let tests = f.root.join("entry.test.ts");
    std::fs::write(
        &entry,
        "import { testLeaf } from './entry.test.ts';\nexport function production() { testLeaf(); }\n",
    )
    .unwrap();
    std::fs::write(
        &tests,
        "import { production } from './entry';\nexport function testLeaf() {}\nexport function testCaller() { production(); }\n",
    )
    .unwrap();
    f.store.cold_build(&[entry.clone(), tests]).unwrap();
    let seeds = resolve_seeds(&f.ctx, &f.root, &[(entry.clone(), 2)], true);
    assert_eq!(seeds.len(), 1);
    let snapshot = f.store.read_snapshot().unwrap();
    assert_eq!(
        snapshot
            .nodes_for(Path::new("entry.test.ts"), "testLeaf")
            .unwrap()
            .len(),
        1
    );
    assert!(
        collect_callgraph_neighbors(&f.ctx, &snapshot, &seeds, false)
            .unwrap()
            .is_empty()
    );
    let included = collect_callgraph_neighbors(&f.ctx, &snapshot, &seeds, true).unwrap();
    assert_eq!(
        included.len(),
        2,
        "tree: {:?}; callers: {:?}",
        crate::commands::callgraph_store_adapter::call_tree_result(
            &snapshot,
            &entry,
            "production",
            1,
            true
        ),
        crate::commands::callgraph_store_adapter::callers_result(
            &snapshot,
            &entry,
            "production",
            1,
            true
        )
    );
    assert!(included
        .iter()
        .all(|node| node.path == Path::new("entry.test.ts")));
}

#[test]
fn gather_graph_off_state_does_not_query_or_build() {
    let f = Fixture::new("fn entry() {}\n");
    let mut config = Config::default();
    config.project_root = Some(f.root.clone());
    config.indexes.callgraph = false;
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    let result = with_graph_snapshot(&ctx, Path::new("unused"), |_| -> Result<(), String> {
        panic!("an off store must not start a walk")
    });
    assert_eq!(result, Err("off".into()));
    assert_eq!(ctx.callgraph_cold_build_spawn_count_for_test(), 0);
}
