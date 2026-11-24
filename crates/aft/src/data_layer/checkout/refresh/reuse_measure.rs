//! Run on Linux with `cargo test -p agent-file-tools --lib -- measure_moved_rust_refresh
//! --ignored --nocapture`. Set `AFT_TEST_MOVED_REPO` to a Git checkout of AFT to
//! use as the source instead of this build's checkout.

use super::super::refresh_cost_measure::timed;
use super::*;
use crate::data_layer::{content::parses_of_for_test, refresh_budget::RECORD};
use std::process::{Command, Output};

fn git(root: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn move_sources(root: &Path) -> (Vec<Vec<u8>>, BTreeMap<ContentKey, Vec<u8>>) {
    let listed = git(root, &["ls-files", "-z", "--", "crates"]).stdout;
    let rust = listed
        .split(|byte| *byte == 0)
        .filter(|path| path.ends_with(b".rs"))
        .collect::<Vec<_>>();
    let count = 500;
    assert!(rust.len() >= count);
    let mut changed = Vec::new();
    let mut sources = BTreeMap::new();
    for i in 0..count {
        let path = std::str::from_utf8(rust[i * rust.len() / count]).unwrap();
        let moved = format!("moved-sources/{path}");
        let bytes = std::fs::read(root.join(path)).unwrap();
        sources.entry(ContentKey::of(&bytes)).or_insert(bytes);
        std::fs::create_dir_all(root.join(&moved).parent().unwrap()).unwrap();
        git(root, &["mv", "--", path, &moved]);
        changed.extend([path.as_bytes().to_vec(), moved.into_bytes()]);
    }
    (changed, sources)
}

fn delta_sections(checkout: &Checkout) -> BTreeMap<(u16, u16), Digest128> {
    let view = checkout.view().unwrap();
    let segment = view.published.segments.last().unwrap();
    let mut sections = BTreeMap::new();
    for (handle, info) in segment.sections() {
        let kind = info.id.kind.get();
        if (0x0200..=0x0306).contains(&kind) {
            let mut bytes = vec![0; info.len.try_into().unwrap()];
            segment.read(handle, 0, &mut bytes).unwrap();
            sections.insert((kind, info.id.key_space.get()), Digest128::of(&bytes));
        }
    }
    assert!(!sections.is_empty());
    sections
}

#[test]
#[ignore = "times two call-graph refreshes of AFT after moving 500 Rust files"]
fn measure_moved_rust_refresh() {
    let repo = std::env::var_os("AFT_TEST_MOVED_REPO").map_or_else(
        || Path::new(crate::test_support::manifest_dir()).join("../.."),
        PathBuf::from,
    );
    let temp = tempfile::tempdir().unwrap();
    let mut results = Vec::new();
    for reuse in [false, true] {
        let name = if reuse { "reuse" } else { "parse" };
        git(
            temp.path(),
            &["clone", "--quiet", "--shared", repo.to_str().unwrap(), name],
        );
        let root = temp.path().join(name);
        let store = tempfile::tempdir().unwrap();
        let checkout = Checkout::bind(
            Arc::new(DataLayer::new(64 << 20, 8)),
            store.path(),
            FamilyId::from_digest(Digest128::of(b"moved Rust measure")),
            &root,
        )
        .unwrap();
        eprintln!("MEASURE {name}: building and warming the call graph");
        let report = checkout.build().unwrap();
        checkout
            .ensure_engine(
                &mut checkout.engine.lock().unwrap(),
                &checkout.view().unwrap().published,
            )
            .unwrap();
        let (paths, sources) = move_sources(&root);
        checkout.record_paths(&paths).unwrap();
        let parses = || {
            sources
                .values()
                .map(|bytes| parses_of_for_test(bytes))
                .sum::<usize>()
        };
        let before = parses();
        let guard = RefreshMemory::start(checkout.family.dir());
        // Moving a file also invalidates callers at other paths. Disable the
        // affected-file limit so both runs time the incremental update, not
        // a cold rebuild triggered by the number of callers to re-resolve.
        *RECORD.lock().unwrap() = Some(Vec::new());
        let (result, timing) = timed(|| {
            checkout.refresh_inner(false, &guard, || {
                if reuse {
                    checkout.family_records().unwrap()
                } else {
                    StoredRecords::default()
                }
            })
        });
        let _ = RECORD.lock().unwrap().take();
        let stats = result.unwrap().unwrap();
        let extractions = parses() - before;
        eprintln!(
            "MEASURE {name}: {timing} moved={} changed={} parsed_before={} grammar_extractions={extractions} cold_rebuild={}",
            paths.len() / 2, stats.changed_paths, report.parsed_files, stats.cold_build
        );
        assert!(!stats.cold_build);
        assert_eq!(stats.changed_paths, paths.len());
        assert_eq!(extractions, if reuse { 0 } else { paths.len() / 2 });
        checkout.publish_changes().unwrap().unwrap();
        results.push(delta_sections(&checkout));
    }
    assert_eq!(results[0], results[1], "publication section bytes changed");
    eprintln!(
        "MEASURE identical publication: {} content/graph sections",
        results[0].len()
    );
}
