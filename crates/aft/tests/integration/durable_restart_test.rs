use aft::blob_store::{BlobPlane, BlobStore, CallgraphKey, FullKey, PutOutcome, SemanticKey};
use aft::views::{
    ArtifactPlane, ClosureRequirements, Manifest, ManifestEntry, PublicationArtifacts,
    PublicationClosure, PublicationObserver, PublicationRequest, PublicationStep, RegularPlanes,
    RelPath, Result as ViewResult, ViewStore,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

const FILE_COUNT: usize = 2_000;
const CHILD_TEST: &str = "durable_restart_test::durable_restart_child";
const FAMILY: &str = "durable-restart-family";
const VIEW: &str = "durable-restart-view";
const ALIAS: &str = "proven-alias";

#[derive(Clone, Copy, Debug)]
struct Failpoint {
    name: &'static str,
    step: PublicationStep,
}

const FAILPOINTS: &[Failpoint] = &[
    Failpoint {
        name: "after-blob-txn-commit",
        step: PublicationStep::BlobWalFsync,
    },
    Failpoint {
        name: "after-wal-checkpoint",
        step: PublicationStep::BlobWalCheckpointed,
    },
    Failpoint {
        name: "after-manifest-file-write",
        step: PublicationStep::ManifestFileWritten,
    },
    Failpoint {
        name: "before-pointer-cas",
        step: PublicationStep::ManifestParentFsynced,
    },
    Failpoint {
        name: "after-pointer-cas",
        step: PublicationStep::PointerCas,
    },
];

#[test]
fn durable_restart_sigkill_each_q5_failpoint_preserves_committed_blobs_and_pointer_closure() {
    for failpoint in FAILPOINTS {
        run_kill9_case(*failpoint, 0x5eed_0000 + failpoint.step as u64);
    }
}

#[test]
fn durable_restart_randomized_schedule_seed_is_replayable() {
    // Keep the seed fixed and include it in failure output so a failing shuffled
    // failpoint order can be reproduced locally.
    let seed = 0x5eed_cafe_u64;
    let mut schedule = FAILPOINTS.to_vec();
    shuffle(&mut schedule, seed);
    for failpoint in schedule {
        run_kill9_case(failpoint, seed);
    }
}

#[test]
#[ignore]
fn durable_restart_child() {
    let root = PathBuf::from(std::env::var_os("DURABLE_RESTART_ROOT").unwrap());
    let storage = PathBuf::from(std::env::var_os("DURABLE_RESTART_STORAGE").unwrap());
    let ready = PathBuf::from(std::env::var_os("DURABLE_RESTART_READY").unwrap());
    let failpoint = std::env::var("DURABLE_RESTART_FAILPOINT").unwrap();
    let step = FAILPOINTS
        .iter()
        .find(|candidate| candidate.name == failpoint)
        .unwrap_or_else(|| panic!("unknown durable restart failpoint {failpoint}"))
        .step;

    let inputs = fixture_inputs(&root, b"new");
    let mut semantic = BlobStore::open(&storage, FAMILY, BlobPlane::Semantic).unwrap();
    let mut callgraph = BlobStore::open(&storage, FAMILY, BlobPlane::Callgraph).unwrap();
    let mut durable = BTreeSet::new();
    for input in &inputs {
        let report = semantic.put(&input.semantic, &input.payload).unwrap();
        if report.outcome == PutOutcome::Inserted && report.durable {
            durable.insert(input.semantic.to_hex());
        }
        let report = callgraph.put(&input.callgraph, &input.payload).unwrap();
        if report.outcome == PutOutcome::Inserted && report.durable {
            durable.insert(input.callgraph.to_hex());
        }
    }
    fs::write(
        storage.join("durable-keys.txt"),
        durable.into_iter().collect::<Vec<_>>().join("\n"),
    )
    .unwrap();

    let artifacts = artifacts(&storage, semantic.path(), callgraph.path());
    let manifest = manifest_for(&inputs);
    let store = ViewStore::open(&storage, VIEW).unwrap();
    let closure = DiskClosure::new(&storage, semantic.path(), callgraph.path(), &artifacts);
    let request = publication_request("new", Some("old"), &manifest, artifacts);
    store
        .publish_with_observer(&request, &closure, Some(&KillAtStep { step, ready }))
        .unwrap();
}

fn run_kill9_case(failpoint: Failpoint, seed: u64) {
    let temp = tempdir().unwrap();
    let root = temp.path().join("root");
    let storage = temp.path().join("storage");
    write_root(&root, b"old");
    seed_old_generation(&root, &storage);
    write_root(&root, b"new");

    let ready = temp.path().join(format!("{}.ready", failpoint.name));
    let mut child = spawn_child(&root, &storage, &ready, failpoint.name);
    wait_for_ready(&mut child, &ready);
    let pid = child.id();
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    assert!(
        !child.wait().unwrap().success(),
        "SIGKILL child exited successfully"
    );

    let inputs = fixture_inputs(&root, b"new");
    let artifacts = artifacts_for_existing(&storage);
    let store = ViewStore::open(&storage, VIEW).unwrap();
    let pointer = store.current_generation().unwrap();
    assert!(
        matches!(pointer.as_deref(), Some("old") | Some("new")),
        "seed={seed} failpoint={} must retain old or publish new pointer: {pointer:?}",
        failpoint.name
    );
    if let Some(generation) = pointer {
        let closure = DiskClosure::new(
            &storage,
            &storage.join("blobs").join(FAMILY).join("semantic.sqlite"),
            &storage.join("blobs").join(FAMILY).join("callgraph.sqlite"),
            &artifacts,
        );
        let requirements = ClosureRequirements {
            referenced_aliases: BTreeSet::from([ALIAS.to_string()]),
        };
        aft::views::probe_publication_closure(
            &store.load_manifest(&generation).unwrap(),
            &requirements,
            &closure,
        )
        .unwrap();
    }

    let durable = fs::read_to_string(storage.join("durable-keys.txt"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut semantic = BlobStore::open(&storage, FAMILY, BlobPlane::Semantic).unwrap();
    let mut callgraph = BlobStore::open(&storage, FAMILY, BlobPlane::Callgraph).unwrap();
    let mut reinserted = BTreeSet::new();
    let mut all_keys = BTreeSet::new();
    for input in &inputs {
        for (store, key) in [
            (&mut semantic, &input.semantic),
            (&mut callgraph, &input.callgraph),
        ] {
            all_keys.insert(key.to_hex());
            let report = store.put(key, &input.payload).unwrap();
            if report.outcome == PutOutcome::Inserted {
                reinserted.insert(key.to_hex());
            }
        }
    }
    // These keys model work that was admitted but had no committed durable row
    // when the daemon was killed; restart is allowed to repeat only these puts.
    for (plane, key) in pending_keys() {
        all_keys.insert(key.to_hex());
        let report = match plane {
            BlobPlane::Semantic => semantic.put(&key, b"pending-after-kill").unwrap(),
            BlobPlane::Callgraph => callgraph.put(&key, b"pending-after-kill").unwrap(),
        };
        if report.outcome == PutOutcome::Inserted {
            reinserted.insert(key.to_hex());
        }
    }
    let expected = all_keys
        .difference(&durable)
        .cloned()
        .collect::<BTreeSet<_>>();
    assert_eq!(
        reinserted, expected,
        "seed={seed} failpoint={} reinserted keys must be exactly uncommitted rows",
        failpoint.name
    );
    assert!(durable.is_disjoint(&reinserted));
}

struct KillAtStep {
    step: PublicationStep,
    ready: PathBuf,
}

impl PublicationObserver for KillAtStep {
    fn reached(&self, step: PublicationStep) {
        if step == self.step {
            fs::write(&self.ready, format!("{step:?}")).unwrap();
            loop {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[derive(Clone)]
struct Input {
    payload: Vec<u8>,
    semantic: FullKey,
    callgraph: FullKey,
}

fn fixture_inputs(root: &Path, flavor: &[u8]) -> Vec<Input> {
    (0..FILE_COUNT)
        .map(|index| {
            let relative = format!("src/bulk/{:04}.rs", index);
            let payload = fs::read(root.join(&relative))
                .unwrap_or_else(|_| [flavor, b"-", index.to_string().as_bytes()].concat());
            Input {
                semantic: SemanticKey::for_current(&payload, relative.as_bytes(), "model")
                    .full_key(),
                callgraph: CallgraphKey::for_current(&payload, "rust").full_key(),
                payload,
            }
        })
        .collect()
}

fn pending_keys() -> Vec<(BlobPlane, FullKey)> {
    vec![
        (
            BlobPlane::Semantic,
            SemanticKey::for_current(b"pending", b"src/pending.rs", "model").full_key(),
        ),
        (
            BlobPlane::Callgraph,
            CallgraphKey::for_current(b"pending", "rust").full_key(),
        ),
    ]
}

fn write_root(root: &Path, flavor: &[u8]) {
    for index in 0..FILE_COUNT {
        let path = root.join(format!("src/bulk/{index:04}.rs"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!(
                "pub fn f_{index}() -> usize {{ {index} }} // {}\n",
                String::from_utf8_lossy(flavor)
            ),
        )
        .unwrap();
    }
}

fn seed_old_generation(root: &Path, storage: &Path) {
    let inputs = fixture_inputs(root, b"old");
    let mut semantic = BlobStore::open(storage, FAMILY, BlobPlane::Semantic).unwrap();
    let mut callgraph = BlobStore::open(storage, FAMILY, BlobPlane::Callgraph).unwrap();
    for input in &inputs {
        semantic.put(&input.semantic, &input.payload).unwrap();
        callgraph.put(&input.callgraph, &input.payload).unwrap();
    }
    let artifacts = artifacts(storage, semantic.path(), callgraph.path());
    let store = ViewStore::open(storage, VIEW).unwrap();
    let manifest = manifest_for(&inputs);
    let closure = DiskClosure::new(storage, semantic.path(), callgraph.path(), &artifacts);
    assert_eq!(
        store
            .publish(
                &publication_request("old", None, &manifest, artifacts),
                &closure,
            )
            .unwrap(),
        aft::views::PublishOutcome::Published
    );
}

fn manifest_for(inputs: &[Input]) -> Manifest {
    Manifest::new(inputs.iter().enumerate().map(|(index, input)| {
        (
            RelPath::new(format!("src/bulk/{index:04}.rs").into_bytes()).unwrap(),
            ManifestEntry::Regular {
                mode: 0o100644,
                planes: RegularPlanes {
                    semantic: Some(input.semantic.to_hex()),
                    callgraph: Some(input.callgraph.to_hex()),
                },
                resolution_input: false,
            },
        )
    }))
    .unwrap()
}

fn artifacts(storage: &Path, semantic: &Path, callgraph: &Path) -> PublicationArtifacts {
    let derived = storage.join("derived.sqlite");
    let aliases = storage.join("oid-alias.sqlite");
    for path in [&derived, &aliases] {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch("CREATE TABLE IF NOT EXISTS state (value TEXT NOT NULL);")
            .unwrap();
    }
    Connection::open(&aliases)
        .unwrap()
        .execute(
            "CREATE TABLE IF NOT EXISTS aliases (git_oid TEXT PRIMARY KEY)",
            [],
        )
        .unwrap();
    Connection::open(&aliases)
        .unwrap()
        .execute("INSERT OR IGNORE INTO aliases VALUES (?1)", params![ALIAS])
        .unwrap();
    let trigram = storage.join("trigram.bin");
    fs::write(&trigram, "ready").unwrap();
    PublicationArtifacts {
        blob_databases: vec![semantic.to_path_buf(), callgraph.to_path_buf()],
        derived_database: derived,
        trigram_artifact: trigram,
        alias_database: aliases,
    }
}

fn artifacts_for_existing(storage: &Path) -> PublicationArtifacts {
    PublicationArtifacts {
        blob_databases: vec![
            storage.join("blobs").join(FAMILY).join("semantic.sqlite"),
            storage.join("blobs").join(FAMILY).join("callgraph.sqlite"),
        ],
        derived_database: storage.join("derived.sqlite"),
        trigram_artifact: storage.join("trigram.bin"),
        alias_database: storage.join("oid-alias.sqlite"),
    }
}

fn publication_request<'a>(
    generation: &'a str,
    base_generation: Option<&'a str>,
    manifest: &'a Manifest,
    artifacts: PublicationArtifacts,
) -> PublicationRequest<'a> {
    PublicationRequest {
        generation,
        base_generation,
        manifest,
        artifacts,
        closure_requirements: ClosureRequirements {
            referenced_aliases: BTreeSet::from([ALIAS.to_string()]),
        },
    }
}

struct DiskClosure {
    semantic_database: PathBuf,
    callgraph_database: PathBuf,
    trigram: PathBuf,
    aliases: PathBuf,
}

impl DiskClosure {
    fn new(
        storage: &Path,
        semantic: &Path,
        callgraph: &Path,
        artifacts: &PublicationArtifacts,
    ) -> Self {
        let _ = storage;
        Self {
            semantic_database: semantic.to_path_buf(),
            callgraph_database: callgraph.to_path_buf(),
            trigram: artifacts.trigram_artifact.clone(),
            aliases: artifacts.alias_database.clone(),
        }
    }
}

impl PublicationClosure for DiskClosure {
    fn contains_blob(&self, plane: ArtifactPlane, full_key: &str) -> ViewResult<bool> {
        let bytes = decode_hex(full_key).unwrap();
        let database = match plane {
            ArtifactPlane::Semantic => &self.semantic_database,
            ArtifactPlane::Callgraph => &self.callgraph_database,
        };
        Ok(Connection::open(database)?
            .query_row(
                "SELECT 1 FROM blob_payloads WHERE full_key = ?1",
                params![bytes],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    fn trigram_is_present(&self) -> ViewResult<bool> {
        Ok(self.trigram.is_file())
    }

    fn contains_alias(&self, git_oid: &str) -> ViewResult<bool> {
        Ok(Connection::open(&self.aliases)?
            .query_row(
                "SELECT 1 FROM aliases WHERE git_oid = ?1",
                params![git_oid],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() % 2 != 0 {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect()
}

fn spawn_child(root: &Path, storage: &Path, ready: &Path, failpoint: &str) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
        .env("DURABLE_RESTART_ROOT", root)
        .env("DURABLE_RESTART_STORAGE", storage)
        .env("DURABLE_RESTART_READY", ready)
        .env("DURABLE_RESTART_FAILPOINT", failpoint)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_for_ready(child: &mut Child, ready: &Path) {
    let started = Instant::now();
    while !ready.is_file() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("durable restart child exited before failpoint: {status}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "timed out waiting for {ready:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn shuffle<T>(items: &mut [T], mut seed: u64) {
    for end in (1..items.len()).rev() {
        seed ^= seed << 7;
        seed ^= seed >> 9;
        items.swap(end, (seed as usize) % (end + 1));
    }
}
