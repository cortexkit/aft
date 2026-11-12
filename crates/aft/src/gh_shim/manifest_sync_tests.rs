use super::super::manifest_wire::Envelope;
use super::*;

fn paths(directory: &tempfile::TempDir) -> StatePaths {
    StatePaths::from_root(directory.path().join("gh-shim"))
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
    assert_eq!(activate(&paths, &equal, now + 1).unwrap().verdict, "valid");
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

#[cfg(unix)]
#[path = "manifest_sync_vault_fake.rs"]
mod vault;
