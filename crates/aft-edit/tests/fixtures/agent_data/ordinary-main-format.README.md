# Ordinary persistence compatibility fixture

`ordinary-main-format.json` was captured from `cortexkit/aft` origin/main at
`be659c9fb8f3876dddc9c93acb06bcbb7b141309` on the Linux build server. The capture
checked out that exact revision, injected disposable unit tests, and ran:

```text
cargo test -p agent-file-tools --lib -- ordinary_main_capture --nocapture
```

Both capture tests passed. The capture asserted main's backup disk version 5,
restore-meta version 2, and checkpoint version 1 before generating any fixture.
The capture script is retained with the delivery under
`.cortexkit/scripts/capture-main-formats.py` (not a production writer or reader).

The normal backup was made through `BackupStore::snapshot_with_op`, followed by
`record_post_mutation_states`. Its output contains the stack document, session
marker, post-state sidecar and actual `to_backup_row` restore metadata. The
`decoded_disk` value came from main's `read_stack_from_disk_unlocked`;
`decoded_db_row` came from main's `BackupEntry::try_from` using that real row.
Main's `restore_metadata_from_json` also accepted the row's version-2 metadata.

The checkpoint was made through `CheckpointStore::create_for_files` and decoded
with main's `read_checkpoint_from_disk`. The binary before-image includes both
a NUL byte and `0xff`. The source file had Unix mode `0640` (the captured mode
33184 also includes the regular-file type bits).

Only the absolute project path, generated backup/blob IDs and clock/order
values were replaced with fixed placeholders. Versions, field presence, flags,
content bytes, post-state digests and decoded semantics were preserved. The
regressions compare the normalized serialized JSON as well as the decoded
values, so an added null/empty guard field is not silently accepted merely
because main's serde reader ignores unknown fields. On non-Unix hosts the mode
fields are null, as in main's platform-specific file-mode capture.

The startup reader-floor regression additionally checks that ordinary
`BackupMeta::written()` stays at the fixture's version 5 while the new build can
still read guarded version 6. Separate imported-only tests require versions
6/3 and checkpoint version 2 when guards are actually present.
