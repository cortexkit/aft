# Windows E2E tests

End-to-end tests for the AFT plugin running inside OpenCode on **native
Windows**. Catches Windows-specific bugs that the Linux Docker harness can't:

- Issue #26 — bash transport timeout headroom on Windows (PowerShell spawn
  is materially slower than `/bin/sh`)
- ONNX runtime install via `tar.exe` (vs `unzip` on Unix)
- Lock-file recovery on Windows (no `isProcessAlive` — falls back to mtime)
- Path-separator handling across trigram index, glob, search
- Windows file URI handling in LSP (`\\?\` extended paths)

## Run paths

There are three ways to run this suite:

| Path                        | Where it runs                       | When to use                            |
|-----------------------------|-------------------------------------|----------------------------------------|
| GitHub Actions              | `windows-2022` runner               | Every PR + push to main (automatic)    |
| Mac → Parallels VM (local)  | Real Windows on your Mac, automated | Reproducing a bug; iterating on a fix  |
| Manual on Windows machine   | Your dev box                        | If you have a Windows dev machine      |

GitHub Actions is the source of truth — it runs automatically on every PR.
The local Parallels path exists for tighter iteration when chasing a bug
without waiting on CI.

## Local: Parallels Desktop on Mac

### Why Parallels and not WSL2

WSL2 is just Linux on Windows kernel — it does **not** catch any of the bugs
this suite is designed to catch (path separators, file locks, signal handling,
process spawn behavior, PowerShell). To test real Windows behavior on a Mac
you need a real Windows VM.

### Why Parallels and not VMware Fusion / UTM / Tart

You already have Parallels installed and it has the smoothest CLI on Apple
Silicon (`prlctl exec`, `prlctl set --shf-host-add`, `prlctl snapshot-switch`).
The orchestrator targets it directly. If you want to use a different tool,
patch `scripts/windows-vm/test.ts` and `scripts/windows-vm/setup.ts`.

### One-time setup (~30-45 minutes)

You need a Windows 11 ARM64 VM in Parallels named `AFT Windows`. If you
have an existing personal Windows VM, **don't reuse it** — create a fresh
one dedicated to AFT testing. Reproducible test infrastructure depends on
a clean baseline that matches what GitHub Actions runners get.

Quick guide for creating the VM:

1. **Get a Windows 11 ARM64 ISO** from Microsoft Eval Center
   ([https://www.microsoft.com/en-us/evalcenter/evaluate-windows-11-enterprise](https://www.microsoft.com/en-us/evalcenter/evaluate-windows-11-enterprise))
   or the Insider Preview ARM64 build.
2. **In Parallels Desktop**: File → New → Install Windows → choose the ISO.
3. **Name the VM `AFT Windows`** (or override with `AFT_WINDOWS_VM_NAME`).
4. **Specs:** 4 vCPU, 8 GB RAM, 60 GB disk.
5. **Skip the Microsoft account requirement** during OOBE: when prompted
   for network, press `Shift+F10` and run `OOBE\BYPASSNRO`. Reboots into
   setup with a "I don't have internet" option that lets you create a
   local account. Local accounts make automation easier.
6. **Install Parallels Tools** when Parallels prompts (Devices menu → Install
   Parallels Tools). Required for `prlctl exec` and shared folders.

Override the VM name if needed:

```bash
export AFT_WINDOWS_VM_NAME="My AFT VM"
```

```bash
# From the repo root on your Mac:
bun run windows-vm:setup
```

This will:
1. Verify Parallels + your VM exist.
2. Mount the repo as a shared folder inside the VM (`Z:\aft-repo\`).
3. Start the VM and tell you to run the guest setup script.
4. Wait for you to confirm guest setup is finished.
5. Take a snapshot named `aft-ready` so future test cycles can revert fast.
6. Suspend the VM.

The guest setup script (`scripts/windows-vm/setup-guest.ps1`) installs Bun,
Rust, Node, OpenCode, aimock, and configures Windows Defender exclusions
that materially speed up cargo builds. **You run this manually inside the
VM** (the orchestrator prints the exact command and waits) because it
requires Administrator rights and several install dialogs are interactive.

### Per-test-run workflow

After one-time setup is done:

```bash
bun run test:windows-e2e
```

Pipeline (~2-3 minutes per run):
1. Mac side reverts the VM to `aft-ready` snapshot.
2. Resumes the VM and waits for Parallels Tools.
3. Re-mounts the shared folder.
4. Inside the VM, runs `scripts/windows-vm/run-guest.ps1`:
   - Mirrors `Z:\aft-repo` into `C:\aft` (skipping `target/`, `node_modules/`)
   - Builds the Rust binary (`cargo build --release`)
   - Builds aft-bridge + aft-opencode dists
   - Runs `tests/windows-e2e/run.ps1`
5. Streams guest output back to your Mac terminal.
6. Suspends the VM (next run resumes from this exact state in ~5s).

The first run after `windows-vm:setup` is slower (~5-10 min) because
cargo + npm caches are cold. Subsequent runs are incremental.

### Environment overrides

| Variable                | Default        | Purpose                                |
|-------------------------|----------------|----------------------------------------|
| `AFT_WINDOWS_VM_NAME`   | `AFT Windows`  | VM name in Parallels                   |
| `AFT_WINDOWS_SNAPSHOT`  | `aft-ready`    | Snapshot to revert to                  |

## Local: Windows dev machine

If you have a Windows machine, skip the VM entirely:

```powershell
# In a clone of cortexkit/aft on the Windows machine:
cargo build --release -p agent-file-tools
bun install
bun --filter "@cortexkit/aft-bridge" run build
bun --filter "@cortexkit/aft-opencode" run build

$env:AFT_BINARY_PATH = "$PWD\target\release\aft.exe"
$env:AFT_PLUGIN_DIST = "$PWD\packages\opencode-plugin\dist"
pwsh -File tests\windows-e2e\run.ps1
```

## Files

| File                                        | What it is                                                |
|---------------------------------------------|-----------------------------------------------------------|
| `tests/windows-e2e/run.ps1`                 | The PowerShell harness — installs deps, runs scenarios    |
| `tests/windows-e2e/mock-server.js`          | aimock mock LLM with scripted tool-call sequence          |
| `scripts/windows-vm/test.ts`                | Mac-side orchestrator: revert + start + exec + suspend    |
| `scripts/windows-vm/run-guest.ps1`          | Guest-side per-cycle: copy + build + run harness          |
| `scripts/windows-vm/setup.ts`               | Mac-side one-time setup orchestrator                      |
| `scripts/windows-vm/setup-guest.ps1`        | Guest-side one-time install (Bun, Rust, Node, OpenCode)   |

## What the suite covers

### Scenario 1 — Full session

Exercises plugin load, bridge spawn, basic tool surface (`aft_outline`,
`read`, `grep`, `edit`, `aft_safety undo`). Catches plugin startup
regressions, bridge transport bugs, and tool-handler bugs that depend on
Windows path semantics.

### Scenario 2 — Bash timeout headroom (issue #26)

Reproduces the issue #26 condition: all `experimental.bash.*` flags on,
multiple bash commands of varying durations with explicit `timeout` values.
Asserts that the bridge transport timeout never fires before bash itself
returns — that's the actual bug class we're protecting against.

The transport timeout calc is `max(30s, requested + 5s)`. The 5s is meant
to absorb process spawn overhead. Scenario 2 includes a 30s sleep with a
60s timeout (transport budget = 65s), so any Windows-side overhead beyond
35s would surface as a bridge timeout in the plugin log.

### Scenario 3 — ONNX install via `tar.exe`

Verifies that the ONNX runtime download path used on Windows (which uses
`tar.exe` for ZIP extraction, not `unzip`) doesn't crash and either installs
successfully or skips gracefully.

## Adding a new scenario

1. Add the new tool-call turn(s) to `tests/windows-e2e/mock-server.js`
   (assign the next sequenceIndex).
2. Add an assertion block to `tests/windows-e2e/run.ps1` that checks the
   plugin log for the expected behavior.
3. If the bug is Windows-specific code, also add a
   `#[cfg(target_os = "windows")]` integration test to
   `crates/aft/tests/integration/bash_windows_test.rs` (or a new file
   following that pattern) so we get fast inner-loop coverage on every
   `cargo test` Windows run.

## Troubleshooting

### `prlctl exec` returns "exec failed"

Parallels Tools probably aren't installed or the VM hasn't fully booted
yet. The orchestrator polls for tool readiness up to 120s; if it gives up,
manually open the VM in Parallels Desktop, log in interactively, install
or repair Parallels Tools, then re-run.

### `Snapshot "aft-ready" not found`

Either you haven't run `bun run windows-vm:setup` yet, or it was
interrupted before the snapshot step. Run it again. If you want to recreate
the snapshot from scratch:

```bash
prlctl snapshot-delete "AFT Windows" --name aft-ready
bun run windows-vm:setup
```

### Cargo builds inside the VM are very slow

Verify Defender exclusions are active:

```powershell
# Inside the VM, in an elevated PowerShell:
Get-MpPreference | Select-Object -ExpandProperty ExclusionPath
```

Should include `C:\aft`, `$env:USERPROFILE\.cargo`, etc. If not,
re-run `setup-guest.ps1` as administrator.
