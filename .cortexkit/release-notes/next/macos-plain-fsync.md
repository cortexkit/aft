On macOS, AFT now flushes its own files with fsync instead of a full drive flush, which makes edits and shell commands faster; set durability.macos_full_sync to true to restore full flushes.
