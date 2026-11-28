use super::super::manifest_wire::Envelope;
use super::*;

#[test]
fn gh_shim_manifest_sync_wakes_when_a_bound_route_launches_with_live_shim_enabled() {
    use std::collections::HashMap;
    use std::sync::mpsc;
    use std::time::Instant;

    // Other tests launch shells too; keep this process-wide worker private.
    const CHILD: &str = "AFT_TEST_GH_SHIM_LIVE_LAUNCH";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gh_shim::manifest_sync::tests::gh_shim_manifest_sync_wakes_when_a_bound_route_launches_with_live_shim_enabled"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let _state = crate::test_env::gh_shim_state_guard();
    let paths = StatePaths::from_process();
    let storage = tempfile::tempdir().unwrap();
    let (requested, requests) = mpsc::channel();
    let worker = spawn_worker(
        storage.path().join("no-vault-connection.json"),
        Box::new(move || {
            let _ = requested.send(());
            Box::pin(async {
                Ok(trust_root::Reply::Root {
                    pin: Some(super::super::tests::fixture_pin()),
                    generation: 1,
                })
            })
        }),
    );
    let mut config = crate::config::Config::default();
    config.git.co_author = "off".into();
    config.github.shim = false;
    let mut environment = HashMap::new();
    crate::agent_child_env::inject(&config, storage.path(), &mut environment, None).unwrap();
    assert!(!environment.contains_key(crate::gh_shim_paths::GH_SHIM_ENABLED_ENV));
    assert_eq!(
        requests.recv_timeout(Duration::from_millis(50)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    assert!(!paths.trust_root.exists());

    // A live config update changes the resolved setting, without a new bind.
    config.github.shim = true;
    crate::agent_child_env::inject(&config, storage.path(), &mut environment, None).unwrap();
    assert_eq!(environment[crate::gh_shim_paths::GH_SHIM_ENABLED_ENV], "1");
    requests
        .recv_timeout(Duration::from_secs(5))
        .expect("the enabled launch should wake the attached worker");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !paths.trust_root.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    crate::agent_child_env::inject(&config, storage.path(), &mut environment, None).unwrap();
    assert!(environment.contains_key(trust_root::ANCHOR_ENV));
    drop(worker);
}

#[test]
fn gh_shim_manifest_sync_notes_missing_vault_list_once_per_log_level() {
    let mut notice = MissingListNotice::default();
    assert_eq!(notice.observe(ListEvent::Missing), (true, false));
    assert_eq!(notice.observe(ListEvent::Missing), (false, false));
    assert_eq!(notice.observe(ListEvent::EnabledRoute), (false, true));
    assert_eq!(notice.observe(ListEvent::Missing), (false, false));
    assert_eq!(notice.observe(ListEvent::EnabledRoute), (false, false));

    let mut recovered = MissingListNotice::default();
    assert_eq!(recovered.observe(ListEvent::Missing), (true, false));
    assert_eq!(recovered.observe(ListEvent::Available), (false, false));
    assert_eq!(recovered.observe(ListEvent::EnabledRoute), (false, false));
}

fn paths(directory: &tempfile::TempDir) -> StatePaths {
    let paths = StatePaths::from_root(directory.path().join("gh-shim"));
    super::super::tests::write_fixture_pin(&paths);
    paths
}

fn candidate(version: u64, now: u64) -> Latest {
    let mut manifest = super::super::tests::fixture_manifest();
    manifest.manifest_version = version;
    let signed = super::super::tests::signed(&manifest, now);
    Latest {
        version,
        envelope: Envelope {
            key_id: signed.key_id,
            manifest_bytes: signed.manifest_bytes,
            signature: signed.signature,
        },
    }
}

#[test]
fn manifest_sync_newer_valid_is_accepted_by_shim_equal_and_older_never_write() {
    let directory = tempfile::tempdir().unwrap();
    let paths = paths(&directory);
    let now = unix_seconds();
    let latest = candidate(29, now);
    let report = activate(&paths, &latest, now).unwrap();
    assert_eq!(report.verdict, "valid");
    assert_eq!(report.version, 29);
    assert_eq!(
        report.payload_sha256,
        digest(&latest.envelope.manifest_bytes)
    );
    assert_eq!(version_high_water(&paths), 29);
    assert_eq!(cached_manifest_report_at(&paths, now).state, Some("valid"));
    let original = fs::read(&paths.manifest).unwrap();
    let mut equal = latest.clone();
    equal.envelope.signature = "not a signature".into();
    assert_eq!(
        activate(&paths, &equal, now + 1).unwrap().verdict,
        "invalid"
    );
    assert_eq!(
        cached_manifest_report_at(&paths, now + 1).state,
        Some("valid")
    );
    activate(&paths, &candidate(28, now), now + 2);
    assert_eq!(fs::read(&paths.manifest).unwrap(), original);
    fs::remove_file(&paths.manifest).unwrap();
    assert!(activate(&paths, &latest, now).is_none());
    assert!(
        !paths.manifest.exists(),
        "no hash is persisted, so equal-version restore is forbidden"
    );
}

#[test]
fn manifest_sync_bad_signature_wrong_key_and_version_mismatch_preserve_previous_file() {
    let directory = tempfile::tempdir().unwrap();
    let paths = paths(&directory);
    let now = unix_seconds();
    activate(&paths, &candidate(28, now), now).unwrap();
    let original = fs::read(&paths.manifest).unwrap();
    let good = candidate(29, now);
    let mut bad_signature = good.clone();
    bad_signature.envelope.signature = base64::engine::general_purpose::STANDARD.encode([0; 64]);
    let mut wrong_key = good.clone();
    wrong_key.envelope.key_id = "0000000000000000".into();
    let mut wrong_version = good;
    wrong_version.version = 30;
    for bad in [bad_signature, wrong_key, wrong_version] {
        let report = activate(&paths, &bad, now).unwrap();
        assert_eq!(report.verdict, "invalid");
        assert!(report.reason.unwrap().len() <= 512);
        assert_eq!(fs::read(&paths.manifest).unwrap(), original);
        assert_eq!(version_high_water(&paths), 28);
    }
}

#[test]
fn manifest_sync_crash_before_atomic_rename_keeps_old_manifest_valid() {
    let directory = tempfile::tempdir().unwrap();
    let paths = paths(&directory);
    let now = unix_seconds();
    activate(&paths, &candidate(28, now), now).unwrap();
    let original = fs::read(&paths.manifest).unwrap();
    let next = candidate(29, now);
    let envelope = SignedManifest {
        artifact_id: MANIFEST_ARTIFACT_ID.into(),
        envelope_version: ENVELOPE_VERSION,
        fetched_at_unix_secs: now,
        key_id: next.envelope.key_id,
        signature: next.envelope.signature,
        manifest_bytes: next.envelope.manifest_bytes,
    };
    assert!(write_candidate(&paths, &envelope, || Err(io::Error::other(
        "crash before rename"
    )))
    .is_err());
    assert_eq!(fs::read(&paths.manifest).unwrap(), original);
    assert_eq!(cached_manifest_report_at(&paths, now).state, Some("valid"));
    assert_eq!(version_high_water(&paths), 28);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for entry in fs::read_dir(&paths.root).unwrap().flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("manifest-sync-")
            {
                assert_eq!(
                    entry.metadata().unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }
}

#[test]
fn manifest_sync_cursor_detects_version_seq_gap_and_epoch_change() {
    let mut cursor = Cursor {
        read_version: Some(29),
        ..Cursor::default()
    };
    let mut notice: Notice = serde_json::from_value(samples()["watch.stored"].clone()).unwrap();
    assert!(!cursor.observe(&notice));
    notice.seq = 3;
    assert!(cursor.observe(&notice));
    notice.seq = 4;
    notice.epoch = "ffeeddccbbaa99887766554433221100".into();
    assert!(cursor.observe(&notice));
    notice.seq = 5;
    notice.version = Some(30);
    assert!(cursor.observe(&notice));
}

fn samples() -> Value {
    serde_json::from_slice::<Value>(include_bytes!(
        "../../../../test-vectors/manifest-binding-v1.json"
    ))
    .unwrap()["samples"]
        .clone()
}

#[test]
fn manifest_sync_health_remembers_newest_notice_even_when_latest_is_older() {
    let mut health = Health::default();
    health.observe_version(Some(29));
    health.observe_version(Some(30));
    health.observe_version(Some(29));
    health.observe_version(None);
    assert_eq!(health.newest_version_seen, Some(30));
}

#[tokio::test(start_paused = true)]
async fn manifest_sync_refresh_retires_old_key_and_deletes_a_cleared_pin() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    let directory = tempfile::tempdir().unwrap();
    let paths = StatePaths::from_root(directory.path().join("gh-shim"));
    let generation = Arc::new(AtomicU64::new(1));
    let read = {
        let generation = generation.clone();
        move || -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<trust_root::Reply, ()>> + Send>> {
            let generation = generation.load(Ordering::Relaxed);
            Box::pin(async move {
                let old = super::super::tests::fixture_pin();
                let pin = match generation {
                    1 => Some(old),
                    2 => {
                        let mut next = super::super::tests::pin_for(&[31; 32]);
                        next.retired_key_ids.push(old.key_id);
                        Some(next)
                    }
                    _ => None,
                };
                Ok(trust_root::Reply::Root { pin, generation })
            })
        }
    };
    let root = RootRefresh::new(&read);
    let now = unix_seconds();
    tokio::select! {
        _ = root.poll(&paths) => panic!("poll stopped"),
        _ = async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            assert_eq!(activate(&paths, &candidate(29, now), now).unwrap().verdict, "valid");
            generation.store(2, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(300)).await;
            assert_eq!(trust_root::read(&paths).unwrap().key_id, super::super::tests::pin_for(&[31; 32]).key_id);
            assert!(matches!(load_manifest(&paths, now), Err(ManifestProblem::Invalid(reason)) if reason.starts_with("retired manifest key id")));
            generation.store(3, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(300)).await;
            assert!(!paths.trust_root.exists());
            assert_eq!(version_high_water(&paths), 29);
        } => {},
    }
}

#[cfg(unix)]
#[path = "manifest_sync_vault_fake.rs"]
mod vault;
