use super::*;
use crate::data_layer::checkout::Checkout;
use crate::data_layer::DataLayer;
use crate::inspect::InspectEntryPoints;
use aft_store::ids::{Digest128, FamilyId};

fn with_view(
    files: impl IntoIterator<Item = (String, &'static str)>,
    test: impl FnOnce(&CheckoutView),
) {
    let root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    for (path, contents) in files {
        std::fs::write(root.path().join(path), contents).unwrap();
    }
    let checkout = Checkout::bind(
        Arc::new(DataLayer::new(8 << 20, 1)),
        store.path(),
        FamilyId::from_digest(Digest128::of(b"streamed dead code")),
        root.path(),
    )
    .unwrap();
    checkout.build().unwrap();
    test(&checkout.view().unwrap());
}

#[test]
fn streamed_dead_code_detects_per_file_export_mismatch() {
    with_view([("lib.ts".into(), "export function kept() {}\n")], |view| {
        let rows = Rows::new(view, &InspectEntryPoints).unwrap();
        let mut rollup = DeadCodeGraph::open_lazy(view.root(), &rows.source);
        rollup.prepare_stream().unwrap();
        let file = rows.file("lib.ts").unwrap();
        let (actual, mut exports, roots) = rollup.stream_file("lib.ts").unwrap();
        assert_eq!(exports, file.exports);
        assert_eq!(exports.pop().unwrap().symbol, "kept");
        assert_eq!(
            compare_file("lib.ts", &file, actual, exports, roots).unwrap_err(),
            "lib.ts: dead-code exports differs from rollup"
        );
    });
}

#[test]
fn streamed_dead_code_detects_cross_file_call_mismatch() {
    with_view(
        [
            (
                "lib.ts".into(),
                "export function used() {}\nexport function unused() {}\n",
            ),
            (
                "main.ts".into(),
                "import { used } from './lib';\nexport function main() { used(); }\n",
            ),
        ],
        |view| {
            let rows = Rows::new(view, &InspectEntryPoints).unwrap();
            let mut rollup = DeadCodeGraph::open_lazy(view.root(), &rows.source);
            rollup.prepare_stream().unwrap();
            let caller = "main.ts";
            let file = rows.file(caller).unwrap();
            let (mut actual, exports, roots) = rollup.stream_file(caller).unwrap();
            let call = actual
                .outbound_calls
                .iter_mut()
                .find(|call| call.target.ends_with("lib.ts::used"))
                .expect("cross-file call to the stored declaration");
            call.target = call.target.replace("::used", "::unused");
            assert_eq!(
                compare_file(caller, &file, actual, exports, roots).unwrap_err(),
                "main.ts: dead-code calls differs from rollup"
            );
        },
    );
}

#[test]
fn streamed_dead_code_drops_full_files_independent_of_file_count() {
    for count in [2, 64] {
        with_view(
            (0..count).map(|index| {
                (
                    format!("file{index:02}.ts"),
                    "export function caller() { target(); }\n",
                )
            }),
            |view| {
                let rows = Rows::new(view, &InspectEntryPoints).unwrap();
                let snapshot = view.dead_code_snapshot_shared(&InspectEntryPoints).unwrap();
                let mut visited = 0;
                rows.compare(&snapshot, |_, file| -> Result<(), String> {
                    visited += 1;
                    assert_eq!(file.calls.len(), 1);
                    assert_eq!(file.exports.len(), 1);
                    Ok(())
                })
                .unwrap();
                // compare checks the number of retained full symbol tables at
                // each visit, rather than only after dropping the graph.
                assert_eq!(visited, count);
            },
        );
    }
}

#[cfg(feature = "test-hooks")]
#[test]
fn streamed_dead_code_artifact_matches_snapshot_json_bytes() {
    use serde_json::json;

    with_view([
        ("lib.ts".into(), "export function sink(value: number) { return value + 1; }\nexport function orphan(value: number) { return value - 1; }\n"),
        ("main.ts".into(), "import { sink } from './lib';\nexport function entry(value: number) { return sink(value); }\n"),
        ("go.mod".into(), "module example.com/exactness\n\ngo 1.23\n"),
        ("iface.go".into(), "package exactness\ntype Required interface { Run() }\ntype Thing struct {}\nfunc (Thing) Run() {}\nfunc invoke(value Required) { value.Run() }\n"),
        ("vars.go".into(), "package exactness\nvar one = 1\nfunc init() {}\n"),
    ], |view| {
        let snapshot = Rows::fixture_snapshot(view, &InspectEntryPoints).unwrap();
        assert!(snapshot.outbound_calls.iter().any(|call| call.target.ends_with("lib.ts::sink")));
        assert!(snapshot.go_initializers.iter().any(|group| !group.targets.is_empty()));
        let initializers = snapshot.go_initializers.iter().map(|group| json!({
            "caller_file": group.caller_file, "callers": group.callers,
            "targets": group.targets.iter().map(|(file, symbol)| (file.as_ref(), symbol.as_ref())).collect::<Vec<_>>()
        })).collect::<Vec<_>>();
        // Build one JSON value for the small fixture so this checks the
        // streamed writer against normal serde_json field and array order.
        let expected = serde_json::to_vec(&json!({
            "files": snapshot.files, "exports": snapshot.exported_symbols,
            "calls": snapshot.outbound_calls, "entry_points": snapshot.entry_points,
            "entry_point_symbols": snapshot.entry_point_symbols, "go_initializers": initializers,
        })).unwrap();
        let out = tempfile::tempdir().unwrap();
        let path = out.path().join("dead-code.json");
        let rows = Rows::new(view, &InspectEntryPoints).unwrap();
        artifact::write(&path, &rows, &view.dead_code_snapshot_shared(&InspectEntryPoints).unwrap()).unwrap();
        let candidate = std::fs::read(path).unwrap();
        assert_eq!(std::str::from_utf8(&candidate).unwrap(), std::str::from_utf8(&expected).unwrap());
    });
}
