use super::*;

#[test]
fn runon_capture_renders_the_old_warning_from_a_record_without_command_names() {
    let dir = tempfile::tempdir().unwrap();
    let (_registry, _task, paths) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("unused-connection"),
        "printf capture",
    );
    let mut old: serde_json::Value =
        serde_json::from_slice(&fs::read(&paths.json).unwrap()).unwrap();
    let remote = old["remote"].as_object_mut().unwrap();
    assert!(!remote.contains_key("copy_warning_commands"));
    assert_eq!(remote.insert("copy_warning".into(), true.into()), None);
    let metadata: PersistedTask = serde_json::from_value(old).unwrap();
    let note = completion_preview::rendered_execution_note(&metadata).unwrap();
    assert_eq!(note.lines().last(), Some("Copied while another command in this checkout was still running; its changes may be missing from the job."));
}
