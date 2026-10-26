use super::*;
use crate::views::manifest_v2::{GenerationName, ManifestHeader, ManifestV2, Producers, PublishV2};

fn fixture() -> (tempfile::TempDir, ViewStore) {
    let storage = tempfile::tempdir().unwrap();
    let directory = crate::blob_store::v3::view_dir(storage.path(), "family", "worker").unwrap();
    crate::reader_floor::publish_v3_with_floor(storage.path(), 3, 1, || {
        std::fs::create_dir_all(&directory).unwrap();
    })
    .unwrap();
    let view = ViewStore::open_dir(directory).unwrap();
    (storage, view)
}

fn manifest(tag: &str) -> ManifestV2 {
    ManifestV2::new_v3(
        ManifestHeader {
            producers: Producers {
                trigram: "trigram".into(),
                semantic: None,
                callgraph: "graph".into(),
            },
            head_tree: None,
            ignore_fingerprint: Some(tag.into()),
            segment: None,
        },
        None,
        Default::default(),
    )
}

fn publish(view: &ViewStore, tag: &str) -> String {
    let manifest = manifest(tag);
    let generation = GenerationName::for_manifest(&manifest).unwrap();
    let previous = view.current_generation_read_only().unwrap();
    assert_eq!(
        view.publish_v2(&generation, previous.as_deref(), &manifest, None)
            .unwrap(),
        PublishV2::Published
    );
    wait_for_generation_sweep_for_test(view.view_dir());
    generation.to_string()
}

fn stage(view: &ViewStore, generation: &str, nonce: &str) -> PathBuf {
    view.view_dir()
        .join(format!(".stat-evidence-{generation}.sqlite.tmp.{nonce}"))
}

#[test]
fn stat_evidence_retires_with_its_generation_but_survives_current_and_read_pins() {
    let (_storage, view) = fixture();
    let old = publish(&view, "old");
    let old_evidence = crate::views::verification::evidence_path(view.view_dir(), &old);
    fs::write(&old_evidence, vec![7; 32_768]).unwrap();
    let old_stage = stage(&view, &old, "abandoned");
    fs::write(&old_stage, b"private evidence").unwrap();
    let marker = crate::root_cache::ReadMarker::create(view.view_dir(), &old).unwrap();
    let current = publish(&view, "current");
    let current_evidence = crate::views::verification::evidence_path(view.view_dir(), &current);
    fs::write(&current_evidence, b"current evidence").unwrap();
    let current_stage = stage(&view, &current, "still-current");
    fs::write(&current_stage, b"current staging").unwrap();
    view.sweep_generations().unwrap();
    assert!(old_evidence.exists());
    assert!(old_stage.exists());
    view.sweep_generations().unwrap();
    assert!(old_evidence.exists());
    assert!(current_evidence.exists());
    assert!(current_stage.exists());
    drop(marker);
    let swept = view.sweep_generations_with_bytes().unwrap();
    assert!(!old_evidence.exists());
    assert!(!old_stage.exists());
    assert!(swept.removed_generations > 0);
    assert!(current_evidence.exists());
    assert!(current_stage.exists());
}

#[test]
fn stat_evidence_staging_never_ages_out_under_a_prepared_generation_pin() {
    let (_storage, view) = fixture();
    publish(&view, "current");
    let manifest = manifest("unpublished");
    let generation = GenerationName::for_manifest(&manifest).unwrap();
    let previous = view.current_generation_read_only().unwrap();
    let prepared = view
        .prepare_v2(&generation, previous.as_deref(), &manifest, None)
        .unwrap();
    let staging = stage(&view, &generation.to_string(), "crashed-writer");
    fs::write(&staging, b"partly written database").unwrap();
    fs::File::open(&staging)
        .unwrap()
        .set_times(
            fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH + Duration::from_secs(60)),
        )
        .unwrap();
    view.sweep_generations().unwrap();
    assert!(staging.exists());
    assert!(view
        .manifest_path(&generation.to_string())
        .unwrap()
        .exists());
    drop(prepared);
    view.sweep_generations().unwrap();
    assert!(!staging.exists());
    assert!(!view
        .manifest_path(&generation.to_string())
        .unwrap()
        .exists());
}

#[test]
fn stat_evidence_orphans_and_sqlite_temporaries_use_existing_removed_byte_accounting() {
    let (_storage, view) = fixture();
    publish(&view, "current");
    let database = crate::views::verification::evidence_path(view.view_dir(), "orphan");
    let journal = database.with_file_name("stat-evidence-orphan.sqlite-journal");
    let temporary = stage(&view, "orphan", "dead-writer");
    let temporary_journal = temporary.with_file_name(format!(
        "{}-journal",
        temporary.file_name().unwrap().to_str().unwrap()
    ));
    for (path, size) in [
        (&database, 65_536),
        (&journal, 4_096),
        (&temporary, 8_192),
        (&temporary_journal, 512),
    ] {
        fs::write(path, vec![3; size]).unwrap();
    }
    assert!(view.has_superseded_artifacts().unwrap());
    let swept = view.sweep_generations_with_bytes().unwrap();
    assert_eq!(swept.removed_bytes, 65_536 + 4_096 + 8_192 + 512);
    assert_eq!(swept.removed_generations, 1);
    for path in [&database, &journal, &temporary, &temporary_journal] {
        assert!(!path.exists());
    }
}

#[test]
fn stat_evidence_discovery_is_confined_to_v3_member_directories() {
    let storage = tempfile::tempdir().unwrap();
    let registry = crate::views::registry::FamilyRegistry::open(storage.path(), "family").unwrap();
    let owner = registry
        .register_view_with_storage(registry.storage(), "legacy", storage.path())
        .unwrap();
    let legacy = owner.view_store().unwrap();
    let evidence = crate::views::verification::evidence_path(legacy.view_dir(), "orphan");
    fs::write(&evidence, b"not a legacy-owned artifact").unwrap();
    assert!(!stat_evidence_namespace(legacy.view_dir()));
    assert!(!legacy.has_superseded_artifacts().unwrap());
    legacy.sweep_generations().unwrap();
    assert_eq!(fs::read(evidence).unwrap(), b"not a legacy-owned artifact");
}
