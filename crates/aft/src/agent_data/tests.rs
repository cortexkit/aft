use super::registry::*;
use cortexkit_project_identity::{EnumerateReply, Outcome, ResolveReply};
use serde_json::json;
use std::path::Path;

fn root_record(root: &Path) -> serde_json::Value {
    json!({"root":root,"remotes":[],"identity":"bound", "incarnation":null,
        "registrationEpoch":null,"approval":{"state":"approved","journalSeq":null},
        "rootKey":{"kind":"label","rootKey":"source"}})
}

fn resolution(root: &Path) -> ResolveReply {
    serde_json::from_value(json!({"projectId":"project-one","via":"root","gone":false,
        "generation":1,"incarnation":"fixture","canonicalRoot":root,
        "rootRecords":[root_record(root)]}))
    .unwrap()
}

fn enumeration(roots: &[&Path]) -> EnumerateReply {
    serde_json::from_value(
        json!({"workspaces":[],"projects":[{"projectId":"project-one",
        "name":"repo","roots":roots,"implicit":false,"refKind":"local",
        "rootRecords":roots.iter().map(|root| root_record(root)).collect::<Vec<_>>()}],
        "generation":1,"incarnation":"fixture"}),
    )
    .unwrap()
}

#[test]
fn move_rebases_by_project_root_key() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("alice/work/repo");
    let destination = temp.path().join("bob/src/repo");
    let decoy = temp.path().join("bob/decoy/repo");
    for root in [&source, &destination, &decoy] {
        std::fs::create_dir_all(root).unwrap();
    }
    let source = source.canonicalize().unwrap();
    let destination = destination.canonicalize().unwrap();
    let decoy = decoy.canonicalize().unwrap();
    std::fs::write(source.join("file.txt"), "postimage").unwrap();
    std::fs::write(destination.join("file.txt"), "postimage").unwrap();
    let key = export_path(&source.join("file.txt"), &resolution(&source)).unwrap();
    let encoded = serde_json::to_string(&key).unwrap();
    assert!(!encoded.contains(source.to_str().unwrap()));
    assert_eq!(key.relative_path, "file.txt");
    let path = import_path(&key, &enumeration(&[&decoy, &destination])).unwrap();
    assert_eq!(path, destination.join("file.txt"));
    assert_ne!(path, source.join("file.txt"));
    assert_eq!(PathDrop::RootUnknown.reason(), "root_unknown");
}

#[test]
fn ambiguous_root_and_no_matching_checkout_are_drops() {
    let temp = tempfile::tempdir().unwrap();
    let one = temp.path().join("one");
    let two = temp.path().join("two");
    for root in [&one, &two] {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("file.txt"), "baseline").unwrap();
    }
    let one = one.canonicalize().unwrap();
    let two = two.canonicalize().unwrap();
    let key = export_path(&one.join("file.txt"), &resolution(&one)).unwrap();
    assert_eq!(
        import_path(&key, &enumeration(&[&one, &two])),
        Err(PathDrop::AmbiguousRoot)
    );
    assert_eq!(
        import_path(&key, &enumeration(&[])),
        Err(PathDrop::RootUnknown)
    );
}

#[test]
fn project_registry_published_vectors_decode() {
    let base =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent_data/project-resolve-v1");
    let manifest = std::fs::read_to_string(base.join("SHA256SUMS")).unwrap();
    use sha2::{Digest, Sha256};
    for line in manifest.lines() {
        let (digest, name) = line.split_once("  ").unwrap();
        let bytes = std::fs::read(base.join(name)).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(&bytes)), digest, "{name}");
        let vectors: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        for vector in vectors {
            let reply = vector["reply"].clone();
            match name {
                "resolve.json" | "resolve-root-records.json" => {
                    serde_json::from_value::<Outcome<ResolveReply>>(reply).unwrap();
                }
                "enumerate.json" => {
                    serde_json::from_value::<Outcome<EnumerateReply>>(reply).unwrap();
                }
                "resolve-project-id.json" => {
                    serde_json::from_value::<
                        Outcome<cortexkit_project_identity::ResolveProjectIdReply>,
                    >(reply)
                    .unwrap();
                }
                "resolve-remote.json" => {
                    serde_json::from_value::<Outcome<cortexkit_project_identity::ResolveRemoteReply>>(reply).unwrap();
                }
                _ => panic!("unexpected vector {name}"),
            }
        }
    }
}

#[test]
fn discovery_is_off_until_operator_enables_it() {
    assert!(!super::CATALOG_ENABLED);
}

#[derive(Debug)]
struct Registry {
    roots: Vec<std::path::PathBuf>,
}
impl super::ProjectRegistry for Registry {
    fn resolve(&self, path: &Path) -> Result<ResolveReply, String> {
        self.roots
            .iter()
            .find(|root| path.starts_with(root))
            .map(|root| resolution(root))
            .ok_or("registry cannot resolve".into())
    }
    fn enumerate(&self) -> Result<EnumerateReply, String> {
        Ok(enumeration(
            &self
                .roots
                .iter()
                .map(|root| root.as_path())
                .collect::<Vec<_>>(),
        ))
    }
}

#[test]
fn imported_undo_preserves_destination_commit() {
    use super::{ExpectedState, FileRecord, SnapshotGroup};
    use crate::backup::BackupStore;
    use std::sync::Arc;
    let temp = tempfile::tempdir().unwrap();
    let checkout = temp.path().canonicalize().unwrap();
    let path = checkout.join("file.txt");
    std::fs::write(&path, "recorded baseline").unwrap();
    let key = export_path(&path, &resolution(&checkout)).unwrap();
    let group = SnapshotGroup::Undo {
        id: "operation-one".into(),
        files: vec![FileRecord {
            path: key,
            id: "entry-one".into(),
            order: 1,
            timestamp: 1,
            mode: None,
            before: Some(b"historical content".to_vec()),
            expected: ExpectedState::capture(&path).unwrap(),
        }],
    };
    let registry = Arc::new(Registry {
        roots: vec![checkout.clone()],
    });
    let storage = checkout.join("aft-storage");
    let mut store = BackupStore::new();
    store.set_storage_dir_for_harness(storage.clone(), crate::harness::Harness::Pi, 0);
    store.set_portable_registry(registry.clone());
    store
        .install_portable_undo("session-one", &[(group, vec![path.clone()])])
        .unwrap();
    // A missing persisted guard must not be hidden by the in-memory entry.
    drop(store);
    let mut store = BackupStore::new();
    store.set_storage_dir_for_harness(storage, crate::harness::Harness::Pi, 0);
    store.set_portable_registry(registry);
    std::fs::write(&path, "destination-only commit").unwrap();
    let before = std::fs::read(&path).unwrap();
    let mut checkpoint_called = false;
    let result = store.restore_latest_detailed("session-one", &path, &mut |_| {
        checkpoint_called = true;
        Ok("external-change-checkpoint".into())
    });
    assert!(
        result.is_err(),
        "imported undo must refuse a divergent destination"
    );
    assert!(
        !checkpoint_called,
        "imported entries must never checkpoint then clobber"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "refusal must write zero project bytes"
    );
    assert_eq!(store.history("session-one", &path).len(), 1);
}

#[test]
fn carried_history_and_move_limits_include_every_stream() {
    use super::snapshot::{CarryBudget, MOVE_LIMIT, SESSION_LIMIT};
    assert_eq!(SESSION_LIMIT, 64 * 1024 * 1024);
    assert_eq!(MOVE_LIMIT, 256 * 1024 * 1024);
    let mut budget = CarryBudget::default();
    assert_eq!(
        budget.add_stream(SESSION_LIMIT + 1, 1),
        Err("session_limit")
    );
    for _ in 0..4 {
        budget.add_stream(SESSION_LIMIT, 1).unwrap();
    }
    assert_eq!(budget.add_stream(1, 1), Err("move_limit"));
    let mut entries = CarryBudget::default();
    entries.add_stream(1, 10_000).unwrap();
    assert_eq!(entries.add_stream(1, 1), Err("entry_limit"));
}

fn registered(root: Option<String>) -> cortexkit_agent_identity::AgentSessionForRefReply {
    serde_json::from_value(json!({"agent_id":"agent-one", "ref_kind":"pi-session",
        "ref":"0123456789abcdef0123456789abcdef", "harness":"pi", "session_id":"native-session",
        "project_root":root, "incarnation":"0123456789abcdef"}))
    .unwrap()
}

#[test]
fn registered_session_requires_exact_native_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let decoy = temp.path().join("repo-decoy");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&decoy).unwrap();
    let registration = registered(Some(root.to_str().unwrap().into()));
    let native = super::binding::NativeIdentity {
        root: decoy,
        harness: "pi".into(),
        session: "native-session".into(),
        storage_harness: crate::harness::Harness::Pi,
    };
    assert!(
        !super::binding::matches_native(&registration, &native),
        "a same-prefix root is another checkout"
    );
}

#[test]
fn registered_session_rejects_different_session_and_null_root() {
    let temp = tempfile::tempdir().unwrap();
    let registration = registered(Some(temp.path().to_str().unwrap().into()));
    let mut native = super::binding::NativeIdentity {
        root: temp.path().into(),
        harness: "pi".into(),
        session: "another-session".into(),
        storage_harness: crate::harness::Harness::Pi,
    };
    assert!(!super::binding::matches_native(&registration, &native));
    native.session = "native-session".into();
    assert!(super::binding::matches_native(&registration, &native));
    assert!(!super::binding::matches_native(&registered(None), &native));
}

#[test]
fn registered_v1_and_new_session_refs_are_never_rederived() {
    for reference in [
        "0123456789abcdef0123456789abcdef",
        "abcdef0123456789abcdef0123456789",
    ] {
        let session = super::participant::ResolvedSession {
            registered_ref: reference.into(),
            project_root: "/unused".into(),
            harness: "pi".into(),
            session: "same-native-session".into(),
            storage_harness: crate::harness::Harness::Pi,
        };
        assert_eq!(session.session_ref().unwrap(), reference);
    }
}

#[test]
fn session_for_ref_published_vectors_decode_without_rewriting() {
    use cortexkit_agent_identity::{AgentSessionForRefParams, AgentSessionForRefReply};
    use cortexkit_project_identity::{RefusalCode, Request};
    use sha2::{Digest, Sha256};
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent_data/agent-sessions-v1");
    let manifest = std::fs::read_to_string(root.join("SHA256SUMS")).unwrap();
    for line in manifest.lines() {
        let (digest, name) = line.split_once("  ").unwrap();
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(std::fs::read(root.join(name)).unwrap())
            ),
            digest
        );
    }
    let vectors: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(root.join("session-for-ref.json")).unwrap()).unwrap();
    for vector in vectors {
        let request: Request<AgentSessionForRefParams> =
            serde_json::from_value(vector["request"].clone()).unwrap();
        assert_eq!(request.method, "agent.session_for_ref");
        let outcome: Outcome<AgentSessionForRefReply> =
            serde_json::from_value(vector["reply"].clone()).unwrap();
        assert_eq!(serde_json::to_value(&outcome).unwrap(), vector["reply"]);
        match outcome {
            Outcome::Success(success) => {
                assert_eq!(success.result.session_ref, request.params.session_ref);
                assert_eq!(success.result.ref_kind, request.params.ref_kind);
            }
            Outcome::Refusal(refusal) => {
                assert_eq!(refusal.code, RefusalCode::NotFound);
                assert!(matches!(
                    super::transport::decode_session_for_ref(vector["reply"].clone()),
                    Err(super::participant::BindingFailure::NotBindable)
                ));
            }
            _ => panic!("unsupported published outcome"),
        }
    }
}

#[test]
fn native_plain_file_history_and_checkpoint_roundtrip_rebase() {
    use super::{DropCounts, SessionSnapshot};
    use crate::{backup::BackupStore, checkpoint::CheckpointStore, harness::Harness};
    use std::sync::Arc;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("alice/work/repo");
    let destination = temp.path().join("bob/src/repo");
    for root in [&source, &destination] {
        std::fs::create_dir_all(root).unwrap();
    }
    let source = source.canonicalize().unwrap();
    let destination = destination.canonicalize().unwrap();
    let source_storage = temp.path().join("source-storage");
    let dest_storage = temp.path().join("destination-storage");
    let source_registry = Registry {
        roots: vec![source.clone()],
    };
    let destination_registry = Arc::new(Registry {
        roots: vec![destination.clone()],
    });
    let path = source.join("file.txt");
    let target = destination.join("file.txt");
    std::fs::write(&path, "old content").unwrap();
    let mut backup = BackupStore::new();
    backup.set_storage_dir_for_harness(source_storage.clone(), Harness::Pi, 0);
    let mut checkpoints = CheckpointStore::new();
    checkpoints.set_storage_dir_for_harness(source_storage, Harness::Pi);
    checkpoints
        .create_for_files("native-session", "before-edit", vec![path.clone()])
        .unwrap();
    backup
        .snapshot_with_op("native-session", &path, "edit", Some("edit-one"))
        .unwrap();
    std::fs::write(&path, "recorded baseline").unwrap();
    backup.record_post_mutation_states();
    std::fs::write(&target, "recorded baseline").unwrap();
    let mut drops = DropCounts::default();
    let mut groups = backup
        .export_portable_undo("native-session", &source_registry, &mut drops)
        .unwrap();
    groups.extend(
        checkpoints
            .export_portable_checkpoints("native-session", &source_registry, &mut drops)
            .unwrap(),
    );
    let bytes = SessionSnapshot { groups }.encode().unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains(source.to_str().unwrap()));
    let decoded = SessionSnapshot::decode(&bytes).unwrap();
    let projection = decoded
        .project(destination_registry.as_ref(), &mut drops)
        .unwrap();
    assert_eq!(projection.len(), 2);
    assert!(drops.0.is_empty());
    let mut imported = BackupStore::new();
    imported.set_storage_dir_for_harness(dest_storage.clone(), Harness::Pi, 0);
    imported.set_portable_registry(destination_registry.clone());
    imported
        .install_portable_undo("native-session", &projection)
        .unwrap();
    let mut imported_checkpoints = CheckpointStore::new();
    imported_checkpoints.set_storage_dir_for_harness(dest_storage.clone(), Harness::Pi);
    imported_checkpoints.set_portable_registry(destination_registry.clone());
    imported_checkpoints
        .install_portable_checkpoints("native-session", &projection)
        .unwrap();
    // Restart both native stores to prove the imported guard is durable.
    drop(imported);
    drop(imported_checkpoints);
    let mut imported = BackupStore::new();
    imported.set_storage_dir_for_harness(dest_storage.clone(), Harness::Pi, 0);
    imported.set_portable_registry(destination_registry.clone());
    imported.restore_latest("native-session", &target).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"old content");
    assert_eq!(std::fs::read(&path).unwrap(), b"recorded baseline");
    let mut imported_checkpoints = CheckpointStore::new();
    imported_checkpoints.set_storage_dir_for_harness(dest_storage, Harness::Pi);
    imported_checkpoints.set_portable_registry(destination_registry);
    std::fs::write(&target, "destination commit").unwrap();
    assert!(imported_checkpoints
        .restore("native-session", "before-edit")
        .is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"destination commit");
    std::fs::write(&target, "recorded baseline").unwrap();
    imported_checkpoints
        .restore("native-session", "before-edit")
        .unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"old content");
}

#[cfg(unix)]
#[test]
fn non_plain_undo_is_dropped_with_reason() {
    use crate::backup::BackupStore;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let link = root.join("link");
    std::os::unix::fs::symlink("missing-target", &link).unwrap();
    let mut backup = BackupStore::new();
    backup
        .snapshot_with_op("session", &link, "symlink", Some("non-plain"))
        .unwrap();
    backup.record_post_mutation_states();
    let mut drops = super::DropCounts::default();
    let groups = backup
        .export_portable_undo("session", &Registry { roots: vec![root] }, &mut drops)
        .unwrap();
    assert!(groups.is_empty());
    assert_eq!(drops.0.get("unsupported_kind"), Some(&1));
}

struct NeverStores;
impl super::participant::SessionStores for NeverStores {
    fn generation(&self, _: &super::participant::ResolvedSession) -> Result<String, String> {
        panic!("state read before admission")
    }
    fn snapshot(
        &self,
        _: &super::participant::ResolvedSession,
        _: &mut super::DropCounts,
    ) -> Result<super::SessionSnapshot, String> {
        panic!("state read before admission")
    }
    fn publish(
        &self,
        _: &super::participant::ResolvedSession,
        _: &[(super::SnapshotGroup, Vec<std::path::PathBuf>)],
    ) -> Result<(), String> {
        panic!("publication before admission")
    }
}

struct CountingResolver(std::sync::atomic::AtomicUsize);
impl super::participant::RefResolver for CountingResolver {
    fn resolve(
        &self,
        _: &cortexkit_role_agent_data::refs::Ref,
    ) -> Result<super::participant::ResolvedSession, super::participant::BindingFailure> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(super::participant::BindingFailure::NotBindable)
    }
}

#[test]
fn non_entorhinal_principal_is_refused_before_any_read() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("not-created");
    let resolver = Arc::new(CountingResolver(AtomicUsize::new(0)));
    let participant = super::participant::Participant::new(
        directory.clone(),
        resolver.clone(),
        Arc::new(super::participant::UnavailableActivity),
        Arc::new(NeverStores),
        Arc::new(Registry { roots: Vec::new() }),
        Arc::new(super::door_tests::MemoryDoor::aft().engram()),
    );
    for principal in ["direct", "reserved:broca", "session:one", "flow:one"] {
        let result = participant
            .handle(principal, "agent_data.describe", &json!({}))
            .unwrap_err();
        assert_eq!(
            result.code,
            cortexkit_role_agent_data::errors::RefusalCode::CallerForbidden
        );
    }
    assert_eq!(resolver.0.load(Ordering::SeqCst), 0);
    assert!(!directory.exists());
}

#[test]
fn control_replies_have_only_contract_known_top_level_fields() {
    use std::sync::Arc;
    let temp = tempfile::tempdir().unwrap();
    let participant = super::participant::Participant::new(
        temp.path().join("state"),
        Arc::new(super::participant::UnbindableRefs),
        Arc::new(super::participant::UnavailableActivity),
        Arc::new(NeverStores),
        Arc::new(Registry { roots: Vec::new() }),
        Arc::new(super::door_tests::MemoryDoor::aft().engram()),
    );
    let reply = participant
        .handle("reserved:entorhinal", "agent_data.describe", &json!({}))
        .unwrap();
    let describe: cortexkit_role_agent_data::describe::DescribeReply =
        serde_json::from_value(reply).unwrap();
    assert_eq!(describe.ref_kinds, ["opencode-session", "pi-session"]);
    let reply = serde_json::to_value(cortexkit_role_agent_data::import::ImportReply::new(
        Vec::new(),
    ))
    .unwrap();
    assert_eq!(reply, json!({"items":[]}));
    let mut with_drops = reply.clone();
    with_drops["dropped_steps_by_reason"] = json!({"unsupported_kind":1});
    assert!(
        serde_json::from_value::<cortexkit_role_agent_data::import::ImportReply>(with_drops)
            .is_err()
    );
}

#[test]
fn published_agent_data_contract_vectors_are_used() {
    use cortexkit_role_agent_data::{request::Request, vectors};
    use sha2::{Digest, Sha256};
    for line in vectors::SHA256SUMS.lines() {
        let (digest, name) = line.split_once("  ").unwrap();
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(vectors::file(name).unwrap().as_bytes())
            ),
            digest,
            "{name}"
        );
    }
    let vectors: serde_json::Value =
        serde_json::from_str(vectors::file("requests.json").unwrap()).unwrap();
    let params = |request: &serde_json::Value| {
        let mut params = request["params"].clone();
        if let Some(count) = request["expand_refs"].as_u64() {
            params["refs"] = json!((0..count)
                .map(|index| json!({"ref_kind":"broca.session","ref":format!("session-{index}")}))
                .collect::<Vec<_>>());
        }
        params
    };
    for request in vectors["requests"].as_array().unwrap() {
        Request::decode(request["method"].as_str().unwrap(), &params(request)).unwrap();
    }
    for request in vectors["refused"].as_array().unwrap() {
        assert_eq!(
            Request::decode(request["method"].as_str().unwrap(), &params(request))
                .unwrap_err()
                .code,
            cortexkit_role_agent_data::errors::RefusalCode::InvalidRequest
        );
    }
}

struct FixtureResolver(super::participant::ResolvedSession);
impl super::participant::RefResolver for FixtureResolver {
    fn resolve(
        &self,
        reference: &cortexkit_role_agent_data::refs::Ref,
    ) -> Result<super::participant::ResolvedSession, super::participant::BindingFailure> {
        if reference.r#ref != self.0.registered_ref {
            return Err(super::participant::BindingFailure::NotBindable);
        }
        Ok(self.0.clone())
    }
}
struct FixtureActivity;
struct FixtureBarrier;
impl super::participant::SessionActivity for FixtureActivity {
    fn acquire(
        &self,
        _: &[super::participant::ResolvedSession],
    ) -> Result<Box<dyn super::participant::SessionBarrier + '_>, String> {
        Ok(Box::new(FixtureBarrier))
    }
}
impl super::participant::SessionBarrier for FixtureBarrier {
    fn busy(
        &self,
        _: &super::participant::ResolvedSession,
    ) -> cortexkit_role_agent_data::busy::BusyEntry {
        cortexkit_role_agent_data::busy::BusyEntry::new(
            cortexkit_role_agent_data::busy::BusyState::Idle,
        )
    }
    fn freeze(&mut self, _: &str, _: &[super::participant::ResolvedSession]) -> Result<(), String> {
        Ok(())
    }
    fn abort(&mut self, _: &str, _: &[super::participant::ResolvedSession]) -> Result<(), String> {
        Ok(())
    }
    fn release(
        &mut self,
        _: &str,
        _: &[super::participant::ResolvedSession],
        _: &str,
    ) -> Result<bool, String> {
        Ok(false)
    }
    fn activate(
        &mut self,
        _: &str,
        _: &[super::participant::ResolvedSession],
    ) -> Result<(), String> {
        Ok(())
    }
}

fn protocol_roundtrip(reference: &str, corrupt: bool) {
    use super::{
        native::NativeStores,
        participant::{Participant, ResolvedSession},
    };
    use crate::{backup::BackupStore, harness::Harness};
    use std::sync::Arc;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source-repo");
    let destination = temp.path().join("receiver-repo");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&destination).unwrap();
    let source = source.canonicalize().unwrap();
    let destination = destination.canonicalize().unwrap();
    let source_storage = temp.path().join("source-storage");
    let dest_storage = temp.path().join("receiver-storage");
    let path = source.join("file.txt");
    let target = destination.join("file.txt");
    std::fs::write(&path, "old bytes").unwrap();
    let mut backups = BackupStore::new();
    backups.set_storage_dir_for_harness(source_storage.clone(), Harness::Pi, 0);
    backups
        .snapshot_with_op("session", &path, "edit", Some("one"))
        .unwrap();
    std::fs::write(&path, "baseline").unwrap();
    backups.record_post_mutation_states();
    drop(backups);
    std::fs::write(&target, "baseline").unwrap();
    let door = super::door_tests::MemoryDoor::aft();
    let source_registry = Arc::new(Registry {
        roots: vec![source.clone()],
    });
    let receiver_registry = Arc::new(Registry {
        roots: vec![destination.clone()],
    });
    let source_identity = ResolvedSession {
        registered_ref: reference.into(),
        project_root: source.clone(),
        harness: "pi".into(),
        session: "session".into(),
        storage_harness: Harness::Pi,
    };
    let receiver_identity = ResolvedSession {
        project_root: destination,
        ..source_identity.clone()
    };
    let sender = Participant::new(
        source_storage.join("agent-data"),
        Arc::new(FixtureResolver(source_identity)),
        Arc::new(FixtureActivity),
        Arc::new(NativeStores {
            storage_root: source_storage,
            registry: source_registry.clone(),
            database: None,
        }),
        source_registry,
        Arc::new(door.engram()),
    );
    let receiver = Participant::new(
        dest_storage.join("agent-data"),
        Arc::new(FixtureResolver(receiver_identity)),
        Arc::new(FixtureActivity),
        Arc::new(NativeStores {
            storage_root: dest_storage.clone(),
            registry: receiver_registry.clone(),
            database: None,
        }),
        receiver_registry.clone(),
        Arc::new(door.engram()),
    );
    let cut = "0123456789abcdef0123456789abcdef";
    let params = json!({"refs":[{"ref_kind":"pi-session","ref":reference}],"cutId":cut});
    let prepared = sender
        .handle("reserved:entorhinal", "agent_data.prepare", &params)
        .unwrap();
    assert_eq!(prepared["items"][0]["sessionRef"], reference);
    assert_eq!(
        sender
            .handle("reserved:entorhinal", "agent_data.prepare", &params)
            .unwrap(),
        prepared
    );
    sender
        .handle("reserved:entorhinal", "agent_data.export", &params)
        .unwrap();
    door.entorhinal_seal(cut, reference, 0);
    if corrupt {
        door.flip_reads();
    }
    let imported = receiver.handle("reserved:entorhinal", "agent_data.import", &params);
    if corrupt {
        assert_eq!(
            imported.unwrap_err().code,
            cortexkit_role_agent_data::errors::RefusalCode::DigestMismatch
        );
        assert!(!dest_storage.join("pi/backups").exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"baseline");
        return;
    }
    let imported = imported.unwrap();
    let typed: cortexkit_role_agent_data::import::ImportReply =
        serde_json::from_value(imported.clone()).unwrap();
    assert_eq!(typed.items[0].session_ref, reference);
    assert_eq!(
        imported.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["items"]
    );
    assert_eq!(
        receiver
            .handle("reserved:entorhinal", "agent_data.import", &params)
            .unwrap(),
        imported
    );
    receiver
        .handle("reserved:entorhinal", "agent_data.activate", &params)
        .unwrap();
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"baseline",
        "import and activation must not write project files"
    );
    let mut backups = BackupStore::new();
    backups.set_storage_dir_for_harness(dest_storage, Harness::Pi, 0);
    backups.set_portable_registry(receiver_registry);
    backups.restore_latest("session", &target).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"old bytes");
    assert_eq!(std::fs::read(&path).unwrap(), b"baseline");
}

#[test]
fn protocol_export_import_keeps_registered_v1_and_new_refs_identical() {
    for reference in [
        "0123456789abcdef0123456789abcdef",
        "abcdef0123456789abcdef0123456789",
    ] {
        protocol_roundtrip(reference, false);
    }
}

#[test]
fn flipped_byte_import_applies_nothing() {
    protocol_roundtrip("0123456789abcdef0123456789abcdef", true);
}

struct FixtureLookup(cortexkit_agent_identity::AgentSessionForRefReply);
impl super::binding::SessionLookup for FixtureLookup {
    fn session_for_ref(
        &self,
        _: &cortexkit_role_agent_data::refs::Ref,
    ) -> Result<cortexkit_agent_identity::AgentSessionForRefReply, super::participant::BindingFailure>
    {
        Ok(self.0.clone())
    }
}
struct FixtureNativeLookup(super::binding::NativeIdentity);
impl super::binding::NativeIdentityLookup for FixtureNativeLookup {
    fn lookup(
        &self,
        _: &Path,
        _: &str,
        _: &str,
    ) -> Result<super::binding::NativeIdentity, super::participant::BindingFailure> {
        Ok(self.0.clone())
    }
}

#[test]
fn entorhinal_with_wrong_native_session_or_null_root_is_refused() {
    use std::sync::Arc;
    let temp = tempfile::tempdir().unwrap();
    let native = super::binding::NativeIdentity {
        root: temp.path().into(),
        harness: "pi".into(),
        session: "different-session".into(),
        storage_harness: crate::harness::Harness::Pi,
    };
    for root in [Some(temp.path().to_str().unwrap().to_owned()), None] {
        let registration = registered(root);
        let params =
            json!({"refs":[{"ref_kind":registration.ref_kind,"ref":registration.session_ref}]});
        let resolver = super::binding::EntorhinalRefs {
            registry: Arc::new(FixtureLookup(registration)),
            native: Arc::new(FixtureNativeLookup(native.clone())),
        };
        let participant = super::participant::Participant::new(
            temp.path().join("participant"),
            Arc::new(resolver),
            Arc::new(super::participant::UnavailableActivity),
            Arc::new(NeverStores),
            Arc::new(Registry { roots: Vec::new() }),
            Arc::new(super::door_tests::MemoryDoor::aft().engram()),
        );
        let refusal = participant
            .handle("reserved:entorhinal", "agent_data.busy", &params)
            .unwrap_err();
        assert_eq!(
            refusal.code,
            cortexkit_role_agent_data::errors::RefusalCode::UnknownRef
        );
    }
}

#[test]
fn portable_deletions_and_created_file_tombstones_roundtrip() {
    use crate::backup::BackupStore;
    use std::sync::Arc;
    for deletion in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        let source = source.canonicalize().unwrap();
        let destination = destination.canonicalize().unwrap();
        let path = source.join("file");
        let target = destination.join("file");
        let mut backup = BackupStore::new();
        if deletion {
            std::fs::write(&path, "deleted content").unwrap();
            backup
                .snapshot_with_op("session", &path, "delete", Some("one"))
                .unwrap();
            std::fs::remove_file(&path).unwrap();
        } else {
            backup
                .snapshot_op_tombstone("session", "one", &path, "create")
                .unwrap();
            std::fs::write(&path, "created content").unwrap();
            std::fs::write(&target, "created content").unwrap();
        }
        backup.record_post_mutation_states();
        let mut drops = super::DropCounts::default();
        let groups = backup
            .export_portable_undo(
                "session",
                &Registry {
                    roots: vec![source],
                },
                &mut drops,
            )
            .unwrap();
        let snapshot =
            super::SessionSnapshot::decode(&super::SessionSnapshot { groups }.encode().unwrap())
                .unwrap();
        let registry = Arc::new(Registry {
            roots: vec![destination],
        });
        let projection = snapshot.project(registry.as_ref(), &mut drops).unwrap();
        assert_eq!(projection.len(), 1);
        assert!(drops.0.is_empty());
        let mut imported = BackupStore::new();
        imported.set_portable_registry(registry);
        imported
            .install_portable_undo("session", &projection)
            .unwrap();
        imported.restore_latest("session", &target).unwrap();
        assert_eq!(target.exists(), deletion);
        if deletion {
            assert_eq!(std::fs::read(&target).unwrap(), b"deleted content");
        }
    }
}

#[test]
fn portable_order_preserves_u128_without_json_number_coercion() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let key = export_path(&root.join("absent"), &resolution(&root)).unwrap();
    let record = super::FileRecord {
        path: key,
        id: "large-order".into(),
        order: u128::MAX,
        timestamp: 1,
        mode: None,
        before: None,
        expected: super::ExpectedState::Absent,
    };
    let original = super::SessionSnapshot {
        groups: vec![super::SnapshotGroup::Undo {
            id: "one".into(),
            files: vec![record],
        }],
    };
    let decoded = super::SessionSnapshot::decode(&original.encode().unwrap()).unwrap();
    assert_eq!(decoded.groups[0].files()[0].order, u128::MAX);
}

#[test]
fn typed_session_lookup_refuses_missing_session_id_and_nullable_harness() {
    use super::binding::{EntorhinalRefs, NativeIdentity};
    use super::participant::{BindingFailure, RefResolver};
    use std::sync::Arc;
    let temp = tempfile::tempdir().unwrap();
    let native = NativeIdentity {
        root: temp.path().into(),
        harness: "pi".into(),
        session: "native-session".into(),
        storage_harness: crate::harness::Harness::Pi,
    };
    let mut wrong_row =
        serde_json::to_value(registered(Some(temp.path().to_str().unwrap().to_owned()))).unwrap();
    wrong_row.as_object_mut().unwrap().remove("session_id");
    wrong_row["session"] = json!("native-session");
    let missing: cortexkit_agent_identity::AgentSessionForRefReply =
        serde_json::from_value(wrong_row).unwrap();
    assert!(missing.session_id.is_none());
    let mut placeholder = registered(Some(temp.path().to_str().unwrap().to_owned()));
    placeholder.harness = None;
    for reply in [missing, placeholder] {
        let reference =
            serde_json::from_value(json!({"ref_kind":reply.ref_kind,"ref":reply.session_ref}))
                .unwrap();
        let resolver = EntorhinalRefs {
            registry: Arc::new(FixtureLookup(reply)),
            native: Arc::new(FixtureNativeLookup(native.clone())),
        };
        assert!(matches!(
            resolver.resolve(&reference),
            Err(BindingFailure::NotBindable)
        ));
    }
}

#[test]
fn typed_session_lookup_not_found_is_final_and_outages_are_unavailable() {
    use super::participant::{BindingFailure, Participant};
    use std::sync::Arc;
    struct LookupReply(serde_json::Value);
    impl super::binding::SessionLookup for LookupReply {
        fn session_for_ref(
            &self,
            _: &cortexkit_role_agent_data::refs::Ref,
        ) -> Result<cortexkit_agent_identity::AgentSessionForRefReply, BindingFailure> {
            super::transport::decode_session_for_ref(self.0.clone())
        }
    }
    let temp = tempfile::tempdir().unwrap();
    for code in ["not_found", "storage_unavailable"] {
        for method in ["agent_data.busy", "agent_data.import"] {
            let resolver = super::binding::EntorhinalRefs {
                registry: Arc::new(LookupReply(
                    json!({"code":code,"message":"lookup refusal","detail":null}),
                )),
                native: Arc::new(FixtureNativeLookup(super::binding::NativeIdentity {
                    root: temp.path().into(),
                    harness: "pi".into(),
                    session: "native-session".into(),
                    storage_harness: crate::harness::Harness::Pi,
                })),
            };
            let participant = Participant::new(
                temp.path().join("participant"),
                Arc::new(resolver),
                Arc::new(super::participant::UnavailableActivity),
                Arc::new(NeverStores),
                Arc::new(Registry { roots: Vec::new() }),
                Arc::new(super::door_tests::MemoryDoor::aft().engram()),
            );
            let mut params = json!({"refs":[{"ref_kind":"pi-session","ref":"0123456789abcdef0123456789abcdef"}]});
            if method == "agent_data.import" {
                params["cutId"] = json!("0123456789abcdef0123456789abcdef");
            }
            let refusal = participant
                .handle("reserved:entorhinal", method, &params)
                .unwrap_err();
            use cortexkit_role_agent_data::errors::RefusalCode;
            let expected = if code != "not_found" {
                RefusalCode::Unavailable
            } else if method == "agent_data.busy" {
                RefusalCode::UnknownRef
            } else {
                RefusalCode::InvalidRequest
            };
            assert_eq!(refusal.code, expected);
        }
    }
}

#[test]
fn imported_undo_reads_back_through_ordinary_agent_paths() {
    use super::{ExpectedState, FileRecord, SnapshotGroup};
    use crate::{backup::BackupStore, harness::Harness};
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join("nested")).unwrap();
    let agent_path = repo.join("file.bin");
    std::fs::write(&agent_path, b"after").unwrap();
    let mut ordinary = BackupStore::new();
    ordinary.set_storage_dir_for_harness(temp.path().join("ordinary"), Harness::Pi, 0);
    ordinary
        .snapshot_with_op("ordinary", &agent_path, "ordinary", Some("ordinary-op"))
        .unwrap();
    ordinary.record_post_mutation_states();
    let ordinary_preview = ordinary
        .preview_latest_path("ordinary", &agent_path)
        .unwrap();
    let mut spellings = Vec::new();
    // A symlinked checkout reproduces macOS's /var versus /private/var spelling
    // on Unix. Windows can expand a plain C:\path spelling to a canonical
    // \\?\C:\path key; parent components cover aliases without symlink rights.
    #[cfg(unix)]
    {
        let alias = temp.path().join("repo-alias");
        std::os::unix::fs::symlink(&repo, &alias).unwrap();
        let alias_path = alias.join("file.bin");
        spellings.push((alias_path.clone(), alias_path));
    }
    spellings.extend([
        (agent_path.clone(), agent_path.clone()),
        (repo.join("nested/../file.bin"), agent_path.clone()),
    ]);
    for (index, (projected_path, lookup_path)) in spellings.into_iter().enumerate() {
        let storage = temp.path().join(format!("imported-{index}"));
        let key = super::registry::PortablePath {
            project_id: "fixture-project".into(),
            root_key: cortexkit_project_identity::RootKey::new(
                cortexkit_project_identity::RootKeyKind::Label,
                "fixture".into(),
            ),
            relative_path: "file.bin".into(),
        };
        let group = SnapshotGroup::Undo {
            id: "imported-op".into(),
            files: vec![FileRecord {
                path: key,
                id: "imported-entry".into(),
                order: 1,
                timestamp: 1,
                mode: None,
                before: Some(b"before".to_vec()),
                expected: ExpectedState::capture(&agent_path).unwrap(),
            }],
        };
        let mut imported = BackupStore::new();
        imported.set_storage_dir_for_harness(storage.clone(), Harness::Pi, 0);
        imported
            .install_portable_undo("imported", &[(group, vec![projected_path.clone()])])
            .unwrap();
        drop(imported);
        let mut imported = BackupStore::new();
        imported.set_storage_dir_for_harness(storage, Harness::Pi, 0);
        let history = imported.history("imported", &lookup_path);
        assert_eq!(history.len(), 1, "ordinary history could not find import: agent={lookup_path:?}, projected={projected_path:?}, ordinary_preview={ordinary_preview:?}");
        assert_eq!(history[0].content_bytes().unwrap().as_ref(), b"before");
        assert_eq!(
            imported
                .preview_latest_path("imported", &lookup_path)
                .unwrap(),
            ordinary_preview
        );
        assert_eq!(
            std::fs::read(&agent_path).unwrap(),
            b"after",
            "read-back and preview must not write project files"
        );
    }
}
