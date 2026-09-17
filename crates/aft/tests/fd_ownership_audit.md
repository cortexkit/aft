# Sandbox-spawn file-descriptor ownership audit

Audit snapshot: `535f277ce80a8e8a1f4a16212132cd36f0613bf9` (2026-09-17).
Line numbers below refer to that snapshot so later edits do not make the evidence ambiguous.

## Live observation and bounded reproduction

The production report recorded several intermittent `EBADF` failures from
`Command::output()` while querying `core.hooksPath`; the report did not preserve an exact
failure count. The daemon had 10,628 open descriptors against a 65,536 soft limit, so this
was not `EMFILE`. Its descriptors 0-2 were intact; low descriptors 6-8 were kqueues and 9+
were sockets.

`concurrent_sandbox_marker_pty_and_git_spawns_preserve_fd_ownership` runs 32 rounds in one
process with four synchronized threads: full native `resolve_sandbox_spawn` profile
resolution (including `resolve_hooks_path`), detached child marker remapping, PTY
open/spawn/close, and independent `Command::output()` plus descriptor churn. It passes on
the audited macOS tree. This is an adversarial bounded reproducer, not a red reproduction of
the reported fault. No close site below was changed merely to manufacture a red test.

## Required-site verdicts

### `crates/aft/src/sandbox_spawn.rs`

- `1972-1975`: `dup` returns a new raw descriptor. On `fdopendir` failure POSIX leaves that
  descriptor with the caller, and the explicit `close` releases it exactly once. Safe.
- `1972, 2037`: after successful `fdopendir`, the `DIR *` exclusively owns the duplicate;
  `closedir` releases it. Neither `readable_fd` nor `parent_fd` owns the duplicate. Safe.
- `2072`: `open("/")` returns a fresh descriptor immediately transferred to `OwnedFd`.
  Safe.
- `2099`, `2126`, `2157`, `2174`, `2197`, `2214`: each successful `openat2`/`openat`
  result is a fresh descriptor immediately transferred to one `OwnedFd`; error paths never
  construct an owner. Safe (Linux-only).
- `2407-2442`: `F_DUPFD_CLOEXEC` creates three independent child-side duplicates. Every
  branch closes each successfully created duplicate once. The closure runs in `pre_exec`,
  after fork, so these closes cannot alter the long-lived daemon's descriptor table. Safe.

### `crates/aft/src/cli/sandbox_launch.rs`

- `237-254`: `read_profile` runs after `exec` in the short-lived launcher. The inherited
  profile descriptor has no surviving Rust owner in that process; `File::from_raw_fd`
  establishes its sole owner and drop closes it before target exec. Safe.

### `crates/aft/src/cli/sandbox_launch/landlock_backend.rs`

- `160-176`: `close_range(5..)` runs in the already-execed Linux launcher, not the daemon.
  The launcher retains only marker descriptors 3-4 and immediately proceeds to target exec.
  It cannot close a daemon descriptor. Safe for the reported macOS failure (Linux-only).
- `238`, `269`, `315`, `330`: successful `open`/`openat2`/`openat` results are fresh and
  immediately transferred to one `OwnedFd`. Safe (Linux-only).

### `crates/aft/src/bash_background/process.rs`

- `464-481`: the only raw closes are in a Unix unit test's `pre_exec` closure. `fcntl`
  creates fresh child-side duplicates, each closed once after `dup2`. No production daemon
  path and no parent-table close. Safe.

### `crates/aft/src/bash_background/persistence.rs`

- `334-365`: `openat` creates `fresh`; failed `fdopendir` leaves ownership with the caller
  and line 347 closes once. Successful `fdopendir` transfers ownership to the `DIR *`, and
  line 365 closes it once. Safe.
- `1822-1826`: successful `openat` returns a fresh descriptor immediately transferred to
  one `File`. Safe.

## Expanded-owner verdicts

- `crates/aft/src/bash_background/registry.rs:6755-6764,6814-6831`: exit, failure, and
  pipeline-status handles are `File::try_clone` results. `apply_marker_fd_allowlist` only
  borrows their raw numbers for post-fork duplication; the parent drops each clone once
  after `spawn`. The registry retains separate original `File` owners. Safe.
- `crates/aft/src/bash_background/pty_process.rs:190-263`: portable-pty objects and cloned
  spill/exit `File`s move to one owner each. There is no raw descriptor storage or close.
  Safe.
- `portable-pty 0.9.0 unix.rs:22-67,228-297`: `openpty` results move once into
  `FileDescriptor`; stdio receives independent duplicates. `close_random_fds` is called
  only from `pre_exec`, hence only in the forked child. Safe with respect to the daemon.
- `crates/aft/src/lsp/client.rs:292-325`: `ChildStdout`, `ChildStdin`, and `ChildStderr` are
  removed with `take` and moved to their reader/writer owners; the retained `Child` no
  longer owns those pipe ends. `kill_lsp_child_group` signals/waits by PID and does not
  close raw descriptors. Safe.
- `crates/aft/src/lsp/manager.rs`: no raw descriptor ownership or close site. Safe.
- `crates/aft/src/agent_child_env.rs`: no raw descriptor ownership or close site. Safe.
- `crates/aft/src/subc/**`: no `RawFd`, `from_raw_fd`, `into_raw_fd`, `dup2`, or
  `libc::close` site. Tokio owns route sockets. No stale integer is retained across an
  await or lock.

## Conclusion

The audited tree contains no parent-process stale close at the reported sandbox/PTY/LSP/subc
sites, and the bounded concurrent test did not reproduce `EBADF`. Consequently this change
does not claim an ownership-site fix. The actionable change is independent: sandbox setup
refusals are now logged at warning level with root, session, and cause, and client text names
`sandbox setup for <root>` before the disable remedy.
