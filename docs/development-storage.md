# Development builds and production storage

AFT's account-owned storage root is `~/.local/share/cortexkit/aft` on Unix
(including macOS), and the OS LocalAppData directory's `cortexkit/aft` on Windows.
The production write fence gets these directories from the OS account database
or Windows known folders. Changing `HOME`, `XDG_*`, `USERPROFILE`, `LOCALAPPDATA`,
`AFT_CACHE_DIR`, or `AFT_STORAGE_DIR` cannot hide that root. Symlink aliases and
not-yet-created descendants are checked against the same root.

Debug assertions identify the builds that tests and development rigs normally
run: plain `cargo test` and the `target/debug/aft` children spawned by Bun tests.
Those builds cannot write the account's production store by default. The fence
uses `cfg!(debug_assertions)` directly; it does not require a release-card
feature, a compile-time environment marker, or a packaging-specific build path.

Optimized release builds, including ordinary `cargo build --release`,
`cargo install`, distro packages and the release-inheriting `stage` profile,
are allowed automatically. Requiring packagers to remember extra flags would
risk shipping an upgrade that refuses every user's store migration. A test or
benchmark that intentionally runs an optimized binary must therefore isolate
its HOME/XDG and storage explicitly; the debug-build fence is not a substitute
for harness isolation.

Debug builds fail closed with `dev_build_refused_production_migration` before
creating or migrating a production database. An existing `aft.db` at exactly
the current schema can be opened **read-only**, without writable-open PRAGMAs.
All production versioned-record writes are refused, not only forward schema
changes. This conservative policy avoids guessing a deployed card's write
format when the reader floor is absent or stale: in particular, a newer build
cannot upgrade bash-task metadata (for example format 6 to 7), publish search,
semantic, symbol or callgraph artifacts, or write backup/checkpoint, ownership,
view/blob/alias/import stores or the reader floor. Use a disposable root for
normal development; production writes are not needed to inspect a matching DB.
The independent Rust test-storage assertion still requires explicit isolated
storage for test contexts.

## Exceptional operator opt-in

For a deliberate production migration, first stop writers and back up the
**whole** storage root, including SQLite WAL files. Then opt in for that one
invocation with `AFT_ALLOW_PRODUCTION_MIGRATION=1`. Only the exact value `1`
authorizes the debug writer. This bypasses the development write fence, not
the newer-reader/downgrade refusal. Do not export it in a shell profile, test
runner, CI job or harness. Prefer placing a reviewed release card instead.

## JavaScript test isolation

The root and package `bunfig.toml` files preload a shared suite setup before any
test code runs. It creates a throwaway HOME, all XDG directories, Windows profile
and LocalAppData directories, and explicit AFT storage/cache directories under
`target/`. The shared child-process wrappers preserve isolated fixture overrides
but replace operator directories, create all directory paths before spawning,
and strip the production-migration opt-in. E2E bridge helpers also provide their
own created per-harness directories; daemon module launches receive these keys
explicitly. Thus direct bridges/pools and direct subprocess tests cannot fall
back to the real HOME simply by omitting `childEnv`.

The suite canary records the account's live `aft.db` schema version and the
sorted table/index catalog (names, types and SQL definitions) before the suite,
and asserts them unchanged afterward, including absence if no DB existed.
SQLite inspection runs read-only in a separate short-lived Bun process, never
as a second fd in a process that may hold a live connection. An unreadable
database fails the canary rather than silently disabling it. Ordinary daemon
record writes and checkpoints change file mtime and size, so those are not
asserted: the canary detects migrations, not normal concurrent production use.
Do not run the suite during a deliberate production schema upgrade, and never
bless a changed schema/catalog snapshot.

When running tests manually, still create and supply a throwaway HOME and
`XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME` before Bun
starts, and unset `AFT_STORAGE_DIR`. Do not bypass the suite preloads.
