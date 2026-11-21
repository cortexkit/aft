use super::*;
use std::time::{Duration, Instant};

pub fn write(root: &Path, path: &str, text: &str) -> PathBuf {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

pub fn root_fixture_config(
    root: &Path,
    storage: &Path,
    edit: impl FnOnce(&mut crate::config::Config),
) -> crate::config::Config {
    write(root, "Cargo.toml", "[package]\nname = \"demo\"\n");
    write(
        root,
        "src/lib.rs",
        "mod util;\npub fn caller() { util::callee(); }\n",
    );
    write(root, "src/util.rs", "pub fn callee() {}\n");
    let mut config = crate::config::Config {
        project_root: Some(root.to_path_buf()),
        storage_dir: Some(storage.to_path_buf()),
        ..crate::config::Config::default()
    };
    edit(&mut config);
    config
}

/// Waits up to 60 s for both planes of `ctx`'s bound checkout to be
/// ready with no edit pending.
pub fn settled(ctx: &AppContext) -> DataStoreReport {
    let started = Instant::now();
    loop {
        let report = report(ctx).expect("data layer on");
        if report.text.status == READY
            && report.callgraph.status == READY
            && report.pending_edits == 0
        {
            return report;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "never settled: {report:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn largest_meaning_fill_health() -> serde_json::Value {
    let fill = MeaningReport {
        plane: MeaningPlane::Serving {
            complete: false,
            refreshing: false,
            coverage: MeaningCoverage {
                files: 123_456,
                files_skipped: 0,
                files_embedded: 100_000,
                files_too_large: 1_234,
                files_failed: 1_234,
                files_grammar_outline_failed: 0,
                files_pending: 20_988,
                files_past_cap: 99_999,
                chunks: 9_999_999,
                chunks_embedded: 8_888_888,
                chunks_failed: 12_345,
            },
            error: Some("embedding backend: the request timed out after 30 s".into()),
        },
        filling: true,
        waiting: true,
        times: crate::data_layer::meaning::FillTimes {
            started_at_ms: Some(1_790_000_000_000),
            stored_at_ms: Some(1_790_000_600_000),
        },
    };
    serde_json::to_value(fill.health()).unwrap()
}
