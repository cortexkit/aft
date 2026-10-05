//! Compare trigram view matches with an independent scan of captured source bytes.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use aft::blob_store::v2::{ContentHash, TrigramKey, TrigramPolicy};
use aft::views::manifest_v2::{EntryPlanes, EntryV2, ManifestHeader, ManifestV2, Producers};
use aft::views::parity_harness::{self, FrozenCheckout, IsolatedStore, PlaneOracle};
use aft::views::readiness::PlaneState;
use aft::views::segment_store::{self, SegmentMember, SegmentReader, TrigramPayload};
use aft::views::snapshot::{
    carry_disk_state_only, derive_successor, LiveDelta, OpenGeneration, WatcherState,
};
use aft::views::trigram::{GenerationIndex, MatchedLine, QueryResult};
use aft::views::{intent, live_delta, RelPath};

fn policy() -> TrigramPolicy {
    TrigramPolicy {
        max_file_size: 1 << 20,
    }
}
fn rel(path: &str) -> RelPath {
    RelPath::new(path.as_bytes().to_vec()).unwrap()
}
fn generation(files: &[(&str, &[u8])], name: &str) -> Arc<OpenGeneration> {
    let mut manifest = ManifestV2::new(ManifestHeader {
        producers: Producers {
            trigram: policy().fingerprint_hex(),
            semantic: None,
            callgraph: "graph-v1".into(),
        },
        head_tree: None,
        ignore_fingerprint: None,
        segment: None,
    });
    for (path, bytes) in files {
        let key = TrigramKey {
            content: ContentHash::of(bytes),
            policy: policy(),
        }
        .family_key();
        manifest
            .insert(
                rel(path),
                EntryV2::regular(
                    ContentHash::of(bytes),
                    bytes.len() as u64,
                    EntryPlanes {
                        trigram: Some(PlaneState::ready(&key)),
                        ..EntryPlanes::default()
                    },
                ),
            )
            .unwrap();
    }
    Arc::new(OpenGeneration::new(name, manifest, None))
}
fn segment(files: &[(&str, &[u8])]) -> Arc<SegmentReader> {
    let bytes = segment_store::assemble(
        &policy(),
        files.iter().map(|(path, bytes)| {
            (
                SegmentMember {
                    rel_path: rel(path),
                    content: ContentHash::of(bytes),
                    size: bytes.len() as u64,
                },
                TrigramPayload::extract(bytes, &policy()),
            )
        }),
    )
    .unwrap();
    Arc::new(SegmentReader::from_bytes(&bytes.bytes).unwrap())
}
fn index(
    segment: Arc<SegmentReader>,
    delta: &LiveDelta,
    files: &[(&str, &[u8])],
) -> GenerationIndex {
    let payloads = files
        .iter()
        .map(|(_, bytes)| {
            (
                ContentHash::of(bytes),
                TrigramPayload::extract(bytes, &policy()),
            )
        })
        .collect();
    GenerationIndex::new(segment, &delta.snapshot(), policy(), &payloads).unwrap()
}
fn write(root: &Path, path: &str, bytes: &[u8]) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

struct Oracle<'a> {
    delta: &'a LiveDelta,
    index: &'a GenerationIndex,
    literal: &'a str,
}
impl PlaneOracle for Oracle<'_> {
    type Observation = QueryResult;
    fn name(&self) -> &str {
        "trigram-matched-lines-and-membership"
    }
    fn observe_view(&self, root: &Path) -> QueryResult {
        self.index.query(root, &self.delta.snapshot(), self.literal)
    }
    fn rebuild_cold(&self, frozen: &FrozenCheckout, store: &IsolatedStore) -> QueryResult {
        let paths = frozen.membership().keys().cloned().collect::<Vec<_>>();
        let rebuilt = segment_store::build_from_files(frozen.root(), &paths, &policy()).unwrap();
        fs::write(store.path().join("cold.seg"), rebuilt.bytes).unwrap();
        // Verify source lines independently, not via the plane's candidate code.
        let mut matches = Vec::new();
        for path in &paths {
            let bytes = frozen.read(path).unwrap();
            for (line, text) in String::from_utf8_lossy(&bytes).lines().enumerate() {
                if text.contains(self.literal) {
                    matches.push(MatchedLine {
                        path: path.clone(),
                        line: line + 1,
                        text: text.into(),
                    });
                }
            }
        }
        QueryResult {
            membership: paths.into_iter().collect(),
            matches,
            gaps: Vec::new(),
        }
    }
}
fn parity(root: &Path, delta: &LiveDelta, index: &GenerationIndex, literal: &str) {
    let scratch = tempfile::tempdir().unwrap();
    parity_harness::check_parity(
        &Oracle {
            delta,
            index,
            literal,
        },
        root,
        scratch.path(),
        literal,
    )
    .unwrap()
    .unwrap();
}

#[test]
fn trigram_parity_edit_and_switch_schedules() {
    for schedule in parity_harness::standard_schedules("base.txt", b"base needle\n", "ignored.txt")
    {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "base.txt", b"base needle\n");
        write(root.path(), ".aftignore", b"ignored.txt\n");
        let files: &[(&str, &[u8])] = &[
            ("base.txt", b"base needle\n"),
            (".aftignore", b"ignored.txt\n"),
        ];
        let shared = segment(files);
        let mut delta = LiveDelta::new(generation(files, "a"));
        let mut plane = index(shared.clone(), &delta, files);
        for step in schedule.steps {
            step.apply(root.path()).unwrap();
            live_delta::reconcile(&mut delta, root.path(), &policy());
            for literal in [
                "needle",
                "edited",
                "branch",
                "ignored",
                "untracked",
                "transient",
            ] {
                parity(root.path(), &delta, &plane, literal);
            }
            // Publish current source into a successor manifest, then switch
            // back to the original manifest without losing the disk edits.
            let current = live_delta::strict_walk(root.path(), &policy(), 0);
            let owned = current
                .entries
                .keys()
                .map(|path| {
                    let name = String::from_utf8(path.as_bytes().to_vec()).unwrap();
                    let bytes = fs::read(root.path().join(&name)).unwrap();
                    (name, bytes)
                })
                .collect::<Vec<_>>();
            let refs = owned
                .iter()
                .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
                .collect::<Vec<_>>();
            let successor = generation(&refs, "b");
            let draft = derive_successor(&delta.cut(), successor, &carry_disk_state_only);
            delta
                .replay_and_swap(draft, &carry_disk_state_only)
                .unwrap();
            plane = index(shared.clone(), &delta, &refs);
            for literal in ["needle", "edited", "ignored"] {
                parity(root.path(), &delta, &plane, literal);
            }
            let draft =
                derive_successor(&delta.cut(), generation(files, "a"), &carry_disk_state_only);
            delta
                .replay_and_swap(draft, &carry_disk_state_only)
                .unwrap();
            plane = index(shared.clone(), &delta, files);
            parity(root.path(), &delta, &plane, "needle");
        }
    }
}

#[test]
fn trigram_pending_intent_scans_before_pruning() {
    let root = tempfile::tempdir().unwrap();
    let files: &[(&str, &[u8])] = &[("a.txt", b"old\n")];
    write(root.path(), "a.txt", b"old\n");
    let mut delta = LiveDelta::new(generation(files, "a"));
    let plane = index(segment(files), &delta, files);
    delta.record_intent(rel("a.txt"));
    write(root.path(), "a.txt", b"new needle\n");
    write(root.path(), "added.txt", b"added needle\n");
    delta.record_intent(rel("added.txt"));
    assert_eq!(
        plane
            .query(root.path(), &delta.snapshot(), "needle")
            .matches
            .len(),
        2
    );
    // Simulate a partial write and rollback without delivering watcher events.
    write(root.path(), "a.txt", b"partial needle\n");
    parity(root.path(), &delta, &plane, "needle");
    write(root.path(), "a.txt", b"old\n");
    parity(root.path(), &delta, &plane, "needle");
}

#[test]
fn trigram_overlay_supersedes_before_posting_pruning() {
    let root = tempfile::tempdir().unwrap();
    let base: &[(&str, &[u8])] = &[("old.txt", b"old\n"), ("gone.txt", b"needle\n")];
    let next: &[(&str, &[u8])] = &[("old.txt", b"needle\n"), ("renamed.txt", b"needle\n")];
    for (path, bytes) in next {
        write(root.path(), path, bytes);
    }
    let delta = LiveDelta::new(generation(next, "b"));
    let plane = index(segment(base), &delta, next);
    let members = delta.snapshot().membership().into_keys().collect();
    let candidates = plane.candidates(&delta.snapshot(), &members, "old");
    assert!(
        !candidates.indexed.contains(&rel("old.txt"))
            && !candidates.direct.contains(&rel("old.txt")),
        "base old posting must be superseded before disk verification"
    );
    parity(root.path(), &delta, &plane, "needle");
}

#[test]
fn trigram_membership_filters_before_presence_guard() {
    let root = tempfile::tempdir().unwrap();
    let files: &[(&str, &[u8])] = &[("a.txt", b"needle\n"), ("ignored.txt", b"needle\n")];
    for (path, bytes) in files {
        write(root.path(), path, bytes);
    }
    let delta = LiveDelta::new(generation(files, "a"));
    let plane = index(segment(files), &delta, files);
    let members = BTreeSet::from([rel("a.txt")]);
    let candidates = plane.candidates(&delta.snapshot(), &members, "needle");
    assert_eq!(
        candidates.indexed, members,
        "ignored file exists but must not become a candidate"
    );
}

#[test]
fn trigram_overflow_hashes_preserved_stat_bytes() {
    let root = tempfile::tempdir().unwrap();
    let files: &[(&str, &[u8])] = &[("a.txt", b"old\n")];
    write(root.path(), "a.txt", b"old\n");
    let mut delta = LiveDelta::new(generation(files, "a"));
    let plane = index(segment(files), &delta, files);
    let mtime = fs::metadata(root.path().join("a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    write(root.path(), "a.txt", b"new\n");
    fs::File::options()
        .write(true)
        .open(root.path().join("a.txt"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    assert_eq!(
        fs::metadata(root.path().join("a.txt"))
            .unwrap()
            .modified()
            .unwrap(),
        mtime
    );
    delta.set_watcher(WatcherState::Overflowed);
    live_delta::reconcile(&mut delta, root.path(), &policy());
    assert_eq!(
        plane
            .query(root.path(), &delta.snapshot(), "new")
            .matches
            .len(),
        1
    );
}

#[test]
fn trigram_healthy_event_hashes_and_reconciles_ignore() {
    let root = tempfile::tempdir().unwrap();
    let files: &[(&str, &[u8])] = &[("a.txt", b"old\n")];
    write(root.path(), "a.txt", b"old\n");
    let mut delta = LiveDelta::new(generation(files, "a"));
    let plane = index(segment(files), &delta, files);
    let mtime = fs::metadata(root.path().join("a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    write(root.path(), "a.txt", b"new\n");
    fs::File::options()
        .write(true)
        .open(root.path().join("a.txt"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    live_delta::apply_event(&mut delta, root.path(), &rel("a.txt"), &policy()).unwrap();
    parity(root.path(), &delta, &plane, "new");
    write(root.path(), ".aftignore", b"a.txt\n");
    live_delta::apply_event(&mut delta, root.path(), &rel(".aftignore"), &policy()).unwrap();
    parity(root.path(), &delta, &plane, "new");
}

#[test]
fn trigram_direct_scan_reports_named_read_gap() {
    let root = tempfile::tempdir().unwrap();
    let files: &[(&str, &[u8])] = &[("a.txt", b"needle\n")];
    let delta = LiveDelta::new(generation(files, "a"));
    let plane = index(segment(files), &delta, files);
    let result = plane.query(root.path(), &delta.snapshot(), "needle");
    assert!(!result.complete());
    assert_eq!(result.gaps, vec![root.path().join("a.txt")]);
}

#[test]
fn trigram_import_mutator_records_intent_without_watcher() {
    use aft::{config::Config, context::AppContext, language::StubProvider, protocol::RawRequest};
    let root = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(root.path()).unwrap();
    let files: &[(&str, &[u8])] = &[("a.ts", b"const x = 1;\n")];
    write(&root, "a.ts", files[0].1);
    let delta = Arc::new(Mutex::new(LiveDelta::new(generation(files, "a"))));
    intent::register(&root, &delta);
    let plane = index(segment(files), &delta.lock().unwrap(), files);
    let ctx = AppContext::new(
        Box::new(StubProvider),
        crate::context_storage::isolate(Config {
            project_root: Some(root.clone()),
            ..Config::default()
        }),
    );
    let request: RawRequest = serde_json::from_value(serde_json::json!({"id":"intent-import", "command":"add_import", "file": root.join("a.ts"), "module":"needle", "names":["thing"]})).unwrap();
    let result = aft::commands::add_import::handle_add_import(&request, &ctx);
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(result["success"], true, "{result}");
    let delta = delta.lock().unwrap();
    assert!(delta
        .snapshot()
        .pending_intent()
        .any(|path| path == &rel("a.ts")));
    assert_eq!(
        plane
            .query(&root, &delta.snapshot(), "needle")
            .matches
            .len(),
        1
    );
}

#[test]
fn trigram_inflight_reconcile_cannot_clear_intent() {
    let root = tempfile::tempdir().unwrap();
    let files: &[(&str, &[u8])] = &[("a.txt", b"old\n")];
    write(root.path(), "a.txt", files[0].1);
    let delta = Arc::new(Mutex::new(LiveDelta::new(generation(files, "a"))));
    intent::register(root.path(), &delta);
    let guard = intent::record_paths([root.path().join("a.txt").as_path()]);
    live_delta::reconcile(&mut delta.lock().unwrap(), root.path(), &policy());
    assert!(delta
        .lock()
        .unwrap()
        .snapshot()
        .pending_intent()
        .next()
        .is_some());
    write(root.path(), "a.txt", b"new\n");
    drop(guard);
    live_delta::reconcile(&mut delta.lock().unwrap(), root.path(), &policy());
    assert!(delta
        .lock()
        .unwrap()
        .snapshot()
        .pending_intent()
        .next()
        .is_none());
}

#[test]
fn trigram_ten_and_forty_view_memory() {
    let corpus = (0..128)
        .map(|file| {
            (
                format!("src/file_{file}.txt"),
                format!("fn example_{file}() {{ needle }}\n")
                    .repeat(64)
                    .into_bytes(),
            )
        })
        .collect::<Vec<_>>();
    let files = corpus
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect::<Vec<_>>();
    let shared = segment(&files);
    let encoded = segment_store::assemble(
        &policy(),
        files.iter().map(|(path, bytes)| {
            (
                SegmentMember {
                    rel_path: rel(path),
                    content: ContentHash::of(bytes),
                    size: bytes.len() as u64,
                },
                TrigramPayload::extract(bytes, &policy()),
            )
        }),
    )
    .unwrap()
    .bytes
    .len();
    let shared_resident = std::mem::size_of::<SegmentReader>()
        + shared
            .files()
            .iter()
            .map(|file| std::mem::size_of_val(file) + file.rel_path.as_bytes().len())
            .sum::<usize>()
        + shared.trigram_count() * std::mem::size_of::<(u32, u64, u32)>()
        + files
            .iter()
            .map(|(_, bytes)| {
                TrigramPayload::extract(bytes, &policy()).records.len()
                    * std::mem::size_of::<segment_store::Posting>()
            })
            .sum::<usize>();
    for count in [10, 40] {
        let views = (0..count)
            .map(|_| {
                let mut delta = LiveDelta::new(generation(&files, "a"));
                for edit in 0..4 {
                    delta.apply(
                        rel(&format!("local_{edit}.txt")),
                        live_delta::entry(b"local needle\n", &policy(), 1),
                    );
                }
                (index(shared.clone(), &delta, &files), delta)
            })
            .collect::<Vec<_>>();
        assert_eq!(Arc::strong_count(&shared), count + 1);
        let checkout_bytes = views
            .iter()
            .map(|(index, delta)| {
                index.checkout_bytes()
                    + std::mem::size_of::<LiveDelta>()
                    + delta
                        .base()
                        .manifest()
                        .entries()
                        .map(|(path, entry)| {
                            path.as_bytes().len()
                                + std::mem::size_of_val(entry)
                                + entry
                                    .plane_state(aft::blob_store::v2::FamilyPlane::Trigram)
                                    .map_or(0, |state| match state {
                                        PlaneState::Ready { key } => key.len(),
                                        _ => 0,
                                    })
                        })
                        .sum::<usize>()
                    + delta
                        .journal()
                        .iter()
                        .map(|record| {
                            std::mem::size_of_val(record) + record.rel_path.as_bytes().len()
                        })
                        .sum::<usize>()
                    + delta
                        .snapshot()
                        .live_entries()
                        .map(|(path, entry)| {
                            path.as_bytes().len()
                                + std::mem::size_of_val(entry)
                                + entry
                                    .attachments
                                    .values()
                                    .filter_map(|a| a.downcast_ref::<live_delta::Attachment>())
                                    .map(|a| {
                                        std::mem::size_of_val(a)
                                            + a.payload.records.len()
                                                * std::mem::size_of::<segment_store::TrigramRecord>(
                                                )
                                    })
                                    .sum::<usize>()
                        })
                        .sum::<usize>()
            })
            .sum::<usize>();
        eprintln!("trigram memory views={count} corpus_files=128 local_edits_per_view=4 shared_segment_encoded_bytes={encoded} shared_segment_resident_estimate={shared_resident} checkout_metadata_and_live_logical_bytes={checkout_bytes} (allocator/BTree node/Arc overhead excluded)");
    }
}

const CHILD: &str = "per_checkout_trigram::trigram_persistence_child";

struct ParkAt {
    step: aft::views::contracts::DurabilityStep,
    ready: std::path::PathBuf,
}
impl aft::views::contracts::DurabilityObserver for ParkAt {
    fn reached(&self, step: aft::views::contracts::DurabilityStep) {
        if step == self.step {
            fs::write(&self.ready, b"ready").unwrap();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }
}

#[test]
#[ignore]
fn trigram_persistence_child() {
    use aft::blob_store::v2::{to_hex, FamilyPlane};
    use aft::views::contracts::DurabilityStep;
    use aft::views::manifest_v2::{GenerationName, PublishV2};
    use aft::views::registry::FamilyRegistry;
    let storage = std::path::PathBuf::from(std::env::var_os("TRIGRAM_STORAGE").unwrap());
    let root = storage.join("checkout");
    fs::create_dir_all(&root).unwrap();
    let registry = FamilyRegistry::open(&storage, "family").unwrap();
    let registration = registry.register_view("scope", &root).unwrap();
    let files: &[(&str, &[u8])] = &[("a.txt", b"old\n")];
    let mut manifest = generation(files, "a").manifest().clone();
    let store = registration.open_store(FamilyPlane::Trigram).unwrap();
    let key = TrigramKey {
        content: ContentHash::of(files[0].1),
        policy: policy(),
    }
    .family_key();
    let mut pin = aft::pins::LivePin::create(&registration).unwrap();
    pin.protect(&[key]).unwrap();
    store
        .put_or_touch(
            &key,
            &TrigramPayload::extract(files[0].1, &policy()).encode(),
        )
        .unwrap();
    let segment = segment_store::assemble(
        &policy(),
        [(
            SegmentMember {
                rel_path: rel("a.txt"),
                content: ContentHash::of(files[0].1),
                size: 4,
            },
            TrigramPayload::extract(files[0].1, &policy()),
        )],
    )
    .unwrap();
    pin.protect_segment(&segment.id).unwrap();
    let park = std::env::var("TRIGRAM_PARK").ok().map(|step| ParkAt {
        step: DurabilityStep::parse(&step).unwrap(),
        ready: storage.join("ready"),
    });
    let observer = park
        .as_ref()
        .map(|park| park as &dyn aft::views::contracts::DurabilityObserver);
    segment_store::write_segment(&store, &storage, &segment, observer).unwrap();
    manifest.header_mut().segment = Some(to_hex(&segment.id));
    let name = GenerationName::for_manifest(&manifest).unwrap();
    let view = registration.view_store().unwrap();
    let prepared = view.prepare_v2(&name, None, &manifest, observer).unwrap();
    assert_eq!(
        view.commit_v2(prepared, observer).unwrap(),
        PublishV2::Published
    );
}

fn spawn_persistence(storage: &Path, park: Option<&str>) -> std::process::Child {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", CHILD, "--ignored", "--nocapture"])
        .env("TRIGRAM_STORAGE", storage);
    if let Some(park) = park {
        command.env("TRIGRAM_PARK", park);
    }
    command.spawn().unwrap()
}

#[test]
fn trigram_real_restart_and_kill_strict_reconcile() {
    use aft::views::contracts::DurabilityStep;
    use aft::views::registry::FamilyRegistry;
    for boundary in [
        None,
        Some(DurabilityStep::SegmentRowRecorded),
        Some(DurabilityStep::SegmentFileSynced),
        Some(DurabilityStep::PointerCas),
    ] {
        let storage = tempfile::tempdir().unwrap();
        let root = storage.path().join("checkout");
        fs::create_dir_all(&root).unwrap();
        write(&root, "a.txt", b"new\n");
        let mut child = spawn_persistence(storage.path(), boundary.map(|step| step.as_str()));
        if boundary.is_some() {
            let started = std::time::Instant::now();
            while !storage.path().join("ready").exists() {
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "child exited before boundary"
                );
                assert!(started.elapsed() < std::time::Duration::from_secs(30));
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            child.kill().unwrap();
            assert!(!child.wait().unwrap().success());
        } else {
            assert!(child.wait().unwrap().success());
        }
        let registry = FamilyRegistry::open(storage.path(), "family").unwrap();
        let view = registry.register_view("scope", &root).unwrap();
        let view_store = view.view_store().unwrap();
        if let Some(current) = view_store.current_generation().unwrap() {
            let manifest = view_store.load_manifest_v2(&current).unwrap();
            let id = aft::blob_store::v2::parse_hex32(manifest.header().segment.as_ref().unwrap())
                .unwrap();
            let path = aft::blob_store::v2::segment_path(storage.path(), "family", &id).unwrap();
            let shared = Arc::new(SegmentReader::open(&path).unwrap());
            let mut delta = LiveDelta::new(Arc::new(OpenGeneration::new(current, manifest, None)));
            delta.set_watcher(WatcherState::Stopped);
            live_delta::reconcile(&mut delta, &root, &policy());
            let plane = index(shared, &delta, &[]);
            parity(&root, &delta, &plane, "new");
            assert_eq!(
                plane.query(&root, &delta.snapshot(), "new").matches.len(),
                1
            );
        } else {
            // Interrupted construction is not ready; a fresh source build recovers.
            assert!(boundary != Some(DurabilityStep::PointerCas));
            let files: &[(&str, &[u8])] = &[("a.txt", b"new\n")];
            let mut delta = LiveDelta::new(generation(files, "recovery"));
            live_delta::reconcile(&mut delta, &root, &policy());
            parity(&root, &delta, &index(segment(files), &delta, files), "new");
        }
    }
}

#[test]
fn trigram_driver_rejects_obsolete_install_and_reports_actual_gaps() {
    use aft::blob_store::v2::FamilyPlane;
    use aft::views::contracts::ViewAccess;
    use aft::views::trigram::{Opener, Publisher, TrigramDriver};
    let storage = tempfile::tempdir().unwrap();
    let root = storage.path().join("checkout");
    fs::create_dir_all(&root).unwrap();
    write(&root, "a.txt", b"old\n");
    let files: &[(&str, &[u8])] = &[("a.txt", b"old\n")];
    let generation = generation(files, "a");
    let producers = generation.manifest().header().producers.clone();
    let delta = Arc::new(Mutex::new(LiveDelta::new(generation)));
    let registry = aft::views::registry::FamilyRegistry::open(storage.path(), "family").unwrap();
    let access = ViewAccess::Owner(registry.register_view("scope", &root).unwrap());
    let shared = segment(files);
    let open: Arc<Opener> = Arc::new(move |_, snapshot| {
        GenerationIndex::new(shared.clone(), snapshot, policy(), &Default::default()).map_err(
            |reason| aft::views::contracts::PlaneError {
                plane: FamilyPlane::Trigram,
                reason,
            },
        )
    });
    let publish: Arc<Publisher> = Arc::new(|_, snapshot, _| Ok(snapshot.generation().clone()));
    let driver = TrigramDriver::new(
        root.clone(),
        "scope".into(),
        producers,
        policy(),
        delta.clone(),
        publish,
        open,
    );
    let observed = driver.reconcile(&access).unwrap();
    let snapshot = delta.lock().unwrap().snapshot();
    delta.lock().unwrap().record_intent(rel("a.txt"));
    assert!(driver
        .install(&access, &snapshot, observed.revision)
        .unwrap_err()
        .reason
        .contains("obsolete"));
    assert!(!driver
        .installed_state(&access, FamilyPlane::Trigram)
        .1
        .is_empty());
    let observed = driver.reconcile(&access).unwrap();
    let snapshot = delta.lock().unwrap().snapshot();
    driver
        .install(&access, &snapshot, observed.revision)
        .unwrap();
    assert!(driver
        .installed_state(&access, FamilyPlane::Trigram)
        .1
        .is_empty());
    assert_eq!(driver.query("old").unwrap().matches.len(), 1);
    delta.lock().unwrap().record_intent(rel("a.txt"));
    assert_eq!(
        driver.installed_state(&access, FamilyPlane::Trigram).1,
        vec![root.join("a.txt")]
    );
}

#[test]
fn trigram_write_success_and_validation_rollback_without_watcher() {
    use aft::{config::Config, context::AppContext, language::StubProvider, protocol::RawRequest};
    let directory = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(directory.path()).unwrap();
    let files: &[(&str, &[u8])] = &[("a.ts", b"const old = 1;\n")];
    write(&root, "a.ts", files[0].1);
    let delta = Arc::new(Mutex::new(LiveDelta::new(generation(files, "a"))));
    intent::register(&root, &delta);
    let plane = index(segment(files), &delta.lock().unwrap(), files);
    let ctx = AppContext::new(
        Box::new(StubProvider),
        crate::context_storage::isolate(Config {
            project_root: Some(root.clone()),
            ..Config::default()
        }),
    );
    let request = |content: &str| {
        serde_json::from_value::<RawRequest>(serde_json::json!({"id":"intent-write", "command":"write", "file":root.join("a.ts"), "content":content})).unwrap()
    };
    let success = aft::commands::write::handle_write(&request("const needle = 2;\n"), &ctx);
    assert_eq!(serde_json::to_value(success).unwrap()["success"], true);
    assert_eq!(
        plane
            .query(&root, &delta.lock().unwrap().snapshot(), "needle")
            .matches
            .len(),
        1
    );
    live_delta::reconcile(&mut delta.lock().unwrap(), &root, &policy());
    let invalid = aft::commands::write::handle_write(&request("const needle = {\n"), &ctx);
    let invalid = serde_json::to_value(invalid).unwrap();
    assert_eq!(invalid["syntax_valid"], false, "{invalid}");
    assert_eq!(
        fs::read_to_string(root.join("a.ts")).unwrap(),
        "const needle = 2;\n"
    );
    assert!(delta
        .lock()
        .unwrap()
        .snapshot()
        .pending_intent()
        .next()
        .is_some());
    parity(&root, &delta.lock().unwrap(), &plane, "needle");
}

#[test]
fn trigram_partial_io_error_keeps_written_bytes_visible() {
    let directory = tempfile::tempdir().unwrap();
    let files: &[(&str, &[u8])] = &[("a.txt", b"old\n")];
    write(directory.path(), "a.txt", files[0].1);
    let delta = Arc::new(Mutex::new(LiveDelta::new(generation(files, "a"))));
    intent::register(directory.path(), &delta);
    let plane = index(segment(files), &delta.lock().unwrap(), files);
    let failed_write = || -> std::io::Result<()> {
        let path = directory.path().join("a.txt");
        let _intent = intent::record_paths([path.as_path()]);
        fs::write(path, b"partial needle\n")?;
        // A real error after the first write models a multi-file mutator's
        // partial failure, rather than pretending the operation was atomic.
        fs::write(directory.path(), b"cannot write a directory")?;
        Ok(())
    };
    assert!(failed_write().is_err());
    let delta = delta.lock().unwrap();
    assert!(delta.snapshot().pending_intent().next().is_some());
    assert_eq!(
        plane
            .query(directory.path(), &delta.snapshot(), "needle")
            .matches
            .len(),
        1
    );
}

#[test]
fn trigram_materializer_preserves_shared_segment_and_other_planes() {
    use aft::blob_store::v2::FamilyPlane;
    use aft::views::contracts::{PlaneAdapter, ViewAccess};
    use aft::views::trigram::{materialize, TrigramAdapter};
    let storage = tempfile::tempdir().unwrap();
    let root = storage.path().join("checkout");
    fs::create_dir_all(&root).unwrap();
    write(&root, "a.txt", b"old\n");
    let registry = aft::views::registry::FamilyRegistry::open(storage.path(), "family").unwrap();
    let registration = registry.register_view("scope", &root).unwrap();
    let access = ViewAccess::Owner(registration.clone());
    let mut manifest = generation(&[("a.txt", b"old\n")], "a").manifest().clone();
    if let EntryV2::Regular { planes, .. } = manifest.get_mut(&rel("a.txt")).unwrap() {
        planes.semantic = Some(PlaneState::pending("model work"));
    }
    let observed = live_delta::strict_walk(&root, &policy(), 1);
    let first = materialize(
        &registration,
        &root,
        &mut manifest,
        &observed.entries,
        policy(),
    )
    .unwrap();
    assert_eq!(
        manifest
            .get(&rel("a.txt"))
            .unwrap()
            .plane_state(FamilyPlane::Semantic),
        Some(&PlaneState::pending("model work"))
    );
    let first_generation = Arc::new(OpenGeneration::new("a", manifest.clone(), None));
    let adapter = TrigramAdapter::new(storage.path().to_path_buf(), policy());
    adapter.open_generation(&access, &first_generation).unwrap();
    fs::rename(root.join("a.txt"), root.join("renamed.txt")).unwrap();
    write(&root, "renamed.txt", b"new needle\n");
    manifest.set(rel("a.txt"), None).unwrap();
    manifest
        .set(
            rel("renamed.txt"),
            generation(&[("renamed.txt", b"new needle\n")], "b")
                .manifest()
                .get(&rel("renamed.txt"))
                .cloned(),
        )
        .unwrap();
    let observed = live_delta::strict_walk(&root, &policy(), 2);
    let next = materialize(
        &registration,
        &root,
        &mut manifest,
        &observed.entries,
        policy(),
    )
    .unwrap();
    assert_eq!(
        first.segment, next.segment,
        "a checkout edit must retain the shared segment"
    );
    let next_generation = Arc::new(OpenGeneration::new("b", manifest, None));
    adapter.open_generation(&access, &next_generation).unwrap();
    let a = adapter.resident("scope", "a").unwrap();
    let b = adapter.resident("scope", "b").unwrap();
    assert!(Arc::ptr_eq(&a.segment, &b.segment));
    let delta = LiveDelta::new(next_generation);
    parity(&root, &delta, &b, "needle");
    assert_eq!(b.query(&root, &delta.snapshot(), "needle").matches.len(), 1);
    adapter.release_generation(&access, "a");
    assert!(adapter.resident("scope", "a").is_none());
}

#[test]
fn trigram_materializer_pins_all_keys_with_one_key_file_write() {
    use aft::pins::work_counters::key_file_work;
    use aft::views::trigram::materialize;
    let storage = tempfile::tempdir().unwrap();
    let root = storage.path().join("checkout");
    fs::create_dir_all(&root).unwrap();
    let contents: Vec<(String, Vec<u8>)> = (0..200)
        .map(|index| {
            (
                format!("f{index:03}.txt"),
                format!("line {index}\n").into_bytes(),
            )
        })
        .collect();
    for (path, bytes) in &contents {
        write(&root, path, bytes);
    }
    let files: Vec<(&str, &[u8])> = contents
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect();
    let registry = aft::views::registry::FamilyRegistry::open(storage.path(), "family").unwrap();
    let registration = registry.register_view("scope", &root).unwrap();
    let mut manifest = generation(&files, "a").manifest().clone();
    let observed = live_delta::strict_walk(&root, &policy(), 1);
    let (writes_before, syncs_before) = key_file_work();
    let materialized = materialize(
        &registration,
        &root,
        &mut manifest,
        &observed.entries,
        policy(),
    )
    .unwrap();
    let (writes_after, syncs_after) = key_file_work();
    // Creating the pin writes an empty key list, the 200 blob keys go in one
    // batch, and the built segment id is pinned last: three key-file writes,
    // independent of the number of files. Keys need no sync: a sweep consumes
    // them only while this process lives, and reclaims the pin after its death.
    assert_eq!(
        (writes_after - writes_before, syncs_after - syncs_before),
        (2, 0),
        "pinning 200 trigram blobs must not rewrite the key file per blob"
    );
    let pinned = fs::read_to_string(materialized.pin.keys_path()).unwrap();
    assert_eq!(pinned.lines().count(), 201);
}
