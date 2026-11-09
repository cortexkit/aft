# Linux inspect LSP lifetime regression

## Reproduction and bisect

Reproduced in Ubuntu 24.04, Linux x86_64 under OrbStack on an arm64 host, with the task worktree mounted at `/work` and isolated Cargo and target volumes. The container uses Rust 1.98.1 and the native C build dependencies described in [the Linux search-readiness investigation](linux-search-readiness.md); Python 3 is also required by the protocol-server fixtures in the broader LSP suite. All builds ran with `CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER=`.

```sh
cargo test --features test-hooks -p agent-file-tools --test integration inspect_command_test::a_typescript_file_still_starts_typescript -- --nocapture
cargo test --features test-hooks -p agent-file-tools --test integration inspect_command_test::blocking_inspect_returns_before_the_configured_request_deadline -- --nocapture
cargo test --features test-hooks -p agent-file-tools --test integration inspect_command_test::blocking_inspect_indexing_gap_names_elapsed_wait_and_retry -- --nocapture
```

At train 235 tip `a32b4e53ff666cd7e84e5e64241ff25888ecb93d`, `a_typescript_file_still_starts_typescript` failed four consecutive runs. Three failed the unscoped assertion (expected active Rust and TypeScript servers); one already failed the scoped assertion (expected an active TypeScript server). Both blocking deadline tests also failed at their reported assertions. CPU load was unnecessary. All three passed on main/train 234, `17f2c807c9f5df1b55e40b4efce3bd54e3262ecc`.

A first-parent `git bisect run` rebuilt and ran the unchanged TypeScript control at each revision:

| Revision | Result |
| --- | --- |
| `b875a2b6621febb673e3b1d3a5b994d8fb6488e1` | fail |
| `13b236ee2aa1fbfdf88b3bb847b5ebb89902962c` | fail |
| `a1be02d65e7604800ab42f3f04c41cfb161c7034` | pass |
| `b0596b3b6217071f07bd4b1f874ff27389411886` | pass |

The first bad commit is `13b236ee2`, “preserve scoped inspect handshakes past wait budget” (the train's first-parent counterpart of `4207b36d0`). Its parent passes. Neither live config reload nor the subsequent handshake-timeout change is required to reproduce the failure.

## Mechanism

`LspClient::spawn_with_reclaim_root` installs `PR_SET_PDEATHSIG(SIGKILL)` on Linux. Linux ties this signal to the **thread that spawned the child**, not just the lifetime of the parent process. A small independent C probe in the same container set the signal in a forked child, acknowledged setup over a pipe, joined the spawning pthread, and waited for the child: it exited with signal 9 while the parent process remained alive.

`13b236ee2` moves process spawn and initialization into a detached continuation in `start_applicable_server_unlocked`. The continuation successfully initializes and registers the client, sends the completed outcome, and exits. Linux then kills the server. The next event drain removes the dead client. Depending on scheduling, the scoped or unscoped assertion sees an empty active-server list. Completed start/quiescence phases describe work that ran, not a guarantee that a server cannot subsequently exit. The deadline tests lose their warming producer, so they do not report the expected still-indexing gap.

This is a product lifetime bug affecting real language servers too, not a premature test observation, fake-server protocol error, config reload, or an inspect-budget cancellation. macOS does not execute the Linux `prctl` branch, explaining the local passes.

## Fix and regression

Linux LSP process creation is dispatched onto one process-lifetime spawning thread. Only spawn and atomic registry registration run there; initialization remains on the existing independent continuation, retaining the bounded inspect wait and allowing slow handshakes to finish later. The spawning thread is retained by a static sender, so a request or continuation finishing cannot trigger parent death. Actual process death still ends the spawning thread and retains the kernel's direct-child SIGKILL protection. Explicit process-group cleanup and the registry's spawn/track lock remain unchanged.

`lsp_remains_usable_after_spawning_thread_exits` creates a real fake-LSP child from a short-lived caller, joins that caller, then performs initialization and graceful shutdown. Before the fix, the test failed with `server must answer after its spawning caller exits: Io(Os { code: 32, kind: BrokenPipe, message: "Broken pipe" })`. After the fix, it passes. The join is the deterministic lifetime boundary; there is no sleep or active-key polling to hide an unusable server. The three original inspect tests retain their assertions unchanged.

## Verification

On the fixed Linux build:

- New lifecycle regression: passed (and failed with `BrokenPipe` on the old implementation).
- `cargo test --features test-hooks -p agent-file-tools --test integration inspect_command_test:: -- --nocapture`: 72 passed, 4 ignored, including all three original failures.
- `cargo test --features test-hooks -p agent-file-tools --test integration lsp_manager_test:: -- --nocapture`: 26 passed, including delayed initialization, initialize timeout/retry, graceful shutdown, and forced group cleanup.
- `cargo test -p agent-file-tools --lib lsp::client:: -- --nocapture`: 14 passed.
- `cargo test -p agent-file-tools --lib lsp::child_registry:: -- --nocapture`: 13 passed.
- `cargo check -p agent-file-tools --tests`: passed.
- Targeted `rustfmt --check` and `git diff --check`: passed.

The first broader manager-suite run had three protocol-fixture failures because this container lacked Python 3 (`#!/usr/bin/env python3`). Installing Python 3 in the container, without changing tests or product code, made all 26 pass. No package manifests or lockfiles changed.
