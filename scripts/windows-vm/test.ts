#!/usr/bin/env bun
/**
 * Run the Windows E2E suite against a local Parallels Windows 11 VM.
 *
 * Pipeline:
 *   1. Verify Parallels + the configured VM exist.
 *   2. Revert to the "aft-ready" snapshot (state with all deps pre-installed).
 *      If the snapshot doesn't exist yet, exit with the setup instructions.
 *   3. Resume / start the VM and wait for guest tools to become responsive.
 *   4. Mount the repo as a shared folder so the guest can read source files
 *      without us shipping a tarball over `prlctl exec`.
 *   5. Inside the VM, run `scripts/windows-vm/run-guest.ps1`, which:
 *        - Copies the shared-folder snapshot to a writable C:\aft directory
 *        - Builds the AFT Rust binary + plugin dist
 *        - Runs tests/windows-e2e/run.ps1
 *   6. Stream the guest stdout back to our terminal.
 *   7. Suspend (don't shutdown) so the next run is fast.
 *
 * One-time setup is documented in `tests/windows-e2e/README.md`. The short
 * version: run `bun run windows-vm:setup` after installing Parallels Desktop
 * and a Windows 11 ARM64 VM.
 *
 * Why no "stop on each run" — Parallels suspend is ~2s vs ~30s cold boot.
 * We expect dozens of iterations during a debug session.
 */

import { spawn, spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";

const REPO_ROOT_FOR_CONFIG = resolve(import.meta.dirname, "..", "..");

// Persistent local override file. Gitignored so each developer can pin their
// VM name once without re-exporting AFT_WINDOWS_VM_NAME every session. Format
// is a single line containing the VM name (no quotes, no shell syntax — we
// just trim and use it).
const LOCAL_VM_NAME_FILE = resolve(REPO_ROOT_FOR_CONFIG, ".aft-windows-vm");

function readLocalVmName(): string | undefined {
  if (!existsSync(LOCAL_VM_NAME_FILE)) return undefined;
  const raw = readFileSync(LOCAL_VM_NAME_FILE, "utf8").trim();
  return raw.length > 0 ? raw : undefined;
}

// VM name resolution order:
//   1. AFT_WINDOWS_VM_NAME env var (explicit override, beats everything)
//   2. .aft-windows-vm file at repo root (gitignored, persistent local pin)
//   3. "AFT Windows" — the recommended name for a dedicated test VM
//
// We don't auto-pick "the only Windows VM" because users often have multiple
// Windows VMs (work, personal). Better to fail with a clear message that
// lists the actual VMs in Parallels and let the user pick.
const VM_NAME = process.env.AFT_WINDOWS_VM_NAME ?? readLocalVmName() ?? "AFT Windows";
const SNAPSHOT_NAME = process.env.AFT_WINDOWS_SNAPSHOT ?? "aft-ready";
const REPO_ROOT = resolve(import.meta.dirname, "..", "..");
const SHARE_NAME = "aft-repo";
// Parallels exposes shared folders at TWO paths inside the Windows guest:
//   - Z:\<share-name>\           — drive-letter mapping; only visible in the
//                                   user's normal token. Invisible to elevated
//                                   processes and to `prlctl exec`'s system-
//                                   context PowerShell because Windows binds
//                                   drive mappings to a single logon session.
//   - \\.psf\<share-name>\        — UNC path; works in any token context
//                                   including elevated PowerShell and
//                                   `prlctl exec`. This is what we use.
//
// Empirical: setup-guest.ps1 hit "drive not found" when run from elevated PS
// at Z:\, but the same content was reachable at \\.psf\aft-repo\.
const SHARE_PATH_IN_GUEST = "\\\\.psf\\aft-repo";

// Resolved during preflight. The Parallels CLI accepts snapshot ids only
// (not names) for `snapshot-switch --id`, so we look up the id once and pass
// it to the revert call. UUID format: {xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}
let resolvedSnapshotId: string | null = null;

interface CmdResult {
  status: number;
  stdout: string;
  stderr: string;
}

function run(cmd: string, args: string[], opts: { silent?: boolean } = {}): CmdResult {
  if (!opts.silent) {
    console.error(`$ ${cmd} ${args.join(" ")}`);
  }
  const result = spawnSync(cmd, args, {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
  return {
    status: result.status ?? -1,
    stdout: result.stdout?.toString() ?? "",
    stderr: result.stderr?.toString() ?? "",
  };
}

function fail(message: string, hint?: string): never {
  console.error(`\n✗ ${message}`);
  if (hint) {
    console.error(`  ${hint}`);
  }
  process.exit(1);
}

function preflight() {
  // Parallels installed?
  const v = run("prlctl", ["--version"], { silent: true });
  if (v.status !== 0) {
    fail(
      "prlctl not found on PATH.",
      "Install Parallels Desktop: https://www.parallels.com/products/desktop/",
    );
  }

  // VM exists?
  //
  // We use --json + os field so we can list ONLY the user's Windows VMs in the
  // failure hint. Other VMs (Linux, macOS test VMs, etc.) aren't useful to
  // suggest for AFT Windows e2e.
  const list = run("prlctl", ["list", "-a", "--json"], { silent: true });
  if (list.status !== 0) {
    fail(`Failed to list Parallels VMs: ${list.stderr.trim()}`);
  }
  let vms: { name: string; os: string }[] = [];
  try {
    vms = JSON.parse(list.stdout) as { name: string; os: string }[];
  } catch (err) {
    fail(
      `Failed to parse 'prlctl list -a --json' output: ${(err as Error).message}`,
      `Raw output:\n${list.stdout}`,
    );
  }
  const vmExists = vms.some((v) => v.name === VM_NAME);
  if (!vmExists) {
    // Surface only Windows VMs in the hint — listing the user's macOS test VMs
    // would be noise. The os field comes from Parallels and is one of "win-11",
    // "win-10", "ubuntu", etc.
    const winVms = vms
      .filter((v) => v.os.startsWith("win"))
      .map((v) => v.name);
    const hintLines = [
      `Pick one of your Windows VMs and pin it for this checkout:`,
      ``,
      ...winVms.map((n) => `    echo "${n}" > ${LOCAL_VM_NAME_FILE}`),
      ``,
      `Or set AFT_WINDOWS_VM_NAME=<name> per invocation.`,
      `If no Windows VM exists yet, create one (Windows 11 ARM64) and run`,
      `\`bun run windows-vm:setup\` to provision it.`,
    ];
    if (winVms.length === 0) {
      // No Windows VMs at all — drop the per-name commands, just show setup.
      fail(
        `VM "${VM_NAME}" not found and no Windows VMs detected in Parallels.`,
        `Install Windows 11 ARM64 first, then run \`bun run windows-vm:setup\`.`,
      );
    }
    fail(
      `VM "${VM_NAME}" not found. Detected Windows VMs: ${winVms.join(", ")}`,
      hintLines.join("\n  "),
    );
  }

  // Snapshot exists?
  //
  // Note: plain `prlctl snapshot-list <vm>` returns only IDs (no names), so a
  // substring match on its stdout for SNAPSHOT_NAME would never hit. We use
  // --json to get a {<id>: {name, date, ...}} map and check the names.
  // The resolved id is stashed in `resolvedSnapshotId` for `snapshot-switch`,
  // which only accepts `--id <uuid>` (not `--name <name>`).
  const snaps = run("prlctl", ["snapshot-list", VM_NAME, "--json"], { silent: true });
  if (snaps.status !== 0) {
    fail(`Failed to list snapshots for "${VM_NAME}": ${snaps.stderr.trim()}`);
  }
  let snapshotsById: Record<string, { name: string }> = {};
  try {
    snapshotsById = JSON.parse(snaps.stdout) as Record<string, { name: string }>;
  } catch (err) {
    fail(
      `Failed to parse 'prlctl snapshot-list --json' output: ${(err as Error).message}`,
      `Raw output:\n${snaps.stdout}`,
    );
  }
  const matched = Object.entries(snapshotsById).find(([, v]) => v.name === SNAPSHOT_NAME);
  if (!matched) {
    const found = Object.values(snapshotsById)
      .map((s) => s.name)
      .join(", ");
    fail(
      `Snapshot "${SNAPSHOT_NAME}" not found on "${VM_NAME}". Found: ${found || "(none)"}`,
      `Run "bun run windows-vm:setup" once to provision the guest and create the snapshot.`,
    );
  }
  resolvedSnapshotId = matched[0];
}

function vmStatus(): "running" | "suspended" | "stopped" | "paused" | "unknown" {
  const r = run("prlctl", ["list", "-a", "--no-header", "-o", "name,status"], {
    silent: true,
  });
  for (const line of r.stdout.split(/\r?\n/)) {
    // Format: "Windows 11                              suspended"
    const trimmed = line.trim();
    if (trimmed.startsWith(VM_NAME)) {
      const status = trimmed.slice(VM_NAME.length).trim();
      if (
        status === "running" ||
        status === "suspended" ||
        status === "stopped" ||
        status === "paused"
      ) {
        return status;
      }
      return "unknown";
    }
  }
  return "unknown";
}

function ensureVmStopped() {
  const status = vmStatus();
  if (status === "running" || status === "paused") {
    console.error(`Stopping VM (state: ${status}) before snapshot revert...`);
    run("prlctl", ["stop", VM_NAME, "--kill"]);
  }
}

function revertSnapshot() {
  if (!resolvedSnapshotId) {
    fail("internal error: revertSnapshot called before preflight resolved snapshot id");
  }
  console.error(
    `Reverting "${VM_NAME}" to snapshot "${SNAPSHOT_NAME}" (${resolvedSnapshotId})...`,
  );
  // Snapshot revert requires the VM to be stopped or suspended. If it's
  // suspended, prlctl handles the revert correctly without a cold boot.
  // `prlctl snapshot-switch <vm> --id <uuid>` is the only supported form;
  // `--name` is rejected with "Unrecognized option" by the current Parallels
  // CLI even though the help text mentions it.
  const r = run("prlctl", ["snapshot-switch", VM_NAME, "--id", resolvedSnapshotId]);
  if (r.status !== 0) {
    fail(`snapshot-switch failed: ${r.stderr.trim() || r.stdout.trim()}`);
  }
}

function startVm() {
  console.error(`Starting VM "${VM_NAME}"...`);
  const r = run("prlctl", ["start", VM_NAME]);
  if (r.status !== 0) {
    fail(`prlctl start failed: ${r.stderr.trim() || r.stdout.trim()}`);
  }
}

async function waitForGuestTools(timeoutSec = 120): Promise<void> {
  // After resume/start, Parallels Tools needs a few seconds to come up before
  // `prlctl exec` works. Poll with a trivial command until it succeeds.
  //
  // We use `--current-user` here to verify the SAME exec mode used for
  // run-guest.ps1 actually works. If the host doesn't support that (older
  // Parallels Tools, the user account isn't a Parallels-known account, etc.),
  // we want to fail readiness here rather than progressing to a confusing
  // failure later inside the build script.
  console.error(`Waiting for Parallels Tools to be ready (up to ${timeoutSec}s)...`);
  const started = Date.now();
  while (Date.now() - started < timeoutSec * 1000) {
    const r = run(
      "prlctl",
      [
        "exec",
        VM_NAME,
        "--current-user",
        "powershell.exe",
        "-NoProfile",
        "-Command",
        "Write-Output ready",
      ],
      { silent: true },
    );
    if (r.status === 0 && r.stdout.includes("ready")) {
      console.error("Parallels Tools ready (--current-user verified).");
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 2000));
  }
  fail(`Parallels Tools did not become ready within ${timeoutSec}s.`);
}

function ensureSharedFolder() {
  // Step 1: enable host-defined sharing globally for the VM. New Parallels
  // VMs default to "Host defined sharing: Off"; without this flag, the
  // shared folder would be ignored even after `--shf-host-add`. Idempotent.
  const enable = run("prlctl", [
    "set",
    VM_NAME,
    "--shared-profile",
    "on",
    "--smart-mount",
    "on",
  ]);
  if (
    enable.status !== 0 &&
    !enable.stderr.toLowerCase().includes("already") &&
    !enable.stderr.toLowerCase().includes("not changed")
  ) {
    // Not fatal — older Parallels versions may not support these flags.
    console.error(
      `  warning: could not enable shared profile: ${enable.stderr.trim() || enable.stdout.trim()}`,
    );
  }

  // Step 2: add the shared folder mapping. Idempotent — `prlctl set`
  // overwrites if the entry already exists.
  console.error(`Mounting repo at ${SHARE_PATH_IN_GUEST} inside the VM...`);
  const r = run("prlctl", [
    "set",
    VM_NAME,
    "--shf-host-add",
    SHARE_NAME,
    "--path",
    REPO_ROOT,
    "--mode",
    "rw",
    "--enable",
  ]);
  if (
    r.status !== 0 &&
    !r.stderr.toLowerCase().includes("already") &&
    !r.stderr.toLowerCase().includes("exists")
  ) {
    fail(`Failed to add shared folder: ${r.stderr.trim() || r.stdout.trim()}`);
  }
}

/**
 * Cross-compile aft.exe + build plugin dists ON THE MAC, before we pass
 * control to the VM. The Windows guest then just consumes pre-built
 * artifacts via the shared folder. See run-guest.ps1's docstring for why
 * we do the heavy lifting on the host.
 */
function buildArtifactsOnHost() {
  // 1. Cross-compile aft.exe (x86_64-pc-windows-gnu).
  console.error("Cross-compiling aft.exe (x86_64-pc-windows-gnu)...");
  const cargoEnv = {
    ...process.env,
    // mingw-w64 linker, installed via `brew install mingw-w64`. Setting via
    // env var rather than cargo config keeps this script self-contained.
    CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER: "x86_64-w64-mingw32-gcc",
  };
  const cargo = spawnSync(
    "cargo",
    [
      "build",
      "--release",
      "--target",
      "x86_64-pc-windows-gnu",
      "-p",
      "agent-file-tools",
    ],
    { cwd: REPO_ROOT, encoding: "utf8", env: cargoEnv, stdio: ["ignore", "inherit", "inherit"] },
  );
  if (cargo.status !== 0) {
    fail(
      `cargo cross-compile failed (exit ${cargo.status}).`,
      "Verify rustup target add x86_64-pc-windows-gnu, brew install mingw-w64, and that cargo build runs locally first.",
    );
  }
  const exePath = resolve(
    REPO_ROOT,
    "target/x86_64-pc-windows-gnu/release/aft.exe",
  );
  // existsSync via fs would be cleaner but spawnSync is enough
  const stat = spawnSync("stat", [exePath], { encoding: "utf8" });
  if (stat.status !== 0) {
    fail(`Cross-build claimed success but ${exePath} not found.`);
  }
  console.error(`  aft.exe ready at ${exePath}`);

  // 2. Build plugin dists with Bun.
  //
  // We run `bun run build` from each package's own directory rather than
  // `bun --filter <pkg> run build` from the workspace root. Bun's --filter
  // CLI flag fails with "No packages matched the filter" for scoped names
  // containing @ (verified empirically against bun 1.3.13 — package.json's
  // own "build" script sidesteps it via `--filter '*'`). Per-package cwd
  // is just as fast and works without the filter glob landmine.
  for (const pkg of ["aft-bridge", "opencode-plugin"]) {
    const pkgDir = resolve(REPO_ROOT, "packages", pkg);
    console.error(`Building ${pkg} dist (in ${pkgDir})...`);
    const bun = spawnSync(
      "bun",
      ["run", "build"],
      { cwd: pkgDir, encoding: "utf8", stdio: ["ignore", "inherit", "inherit"] },
    );
    if (bun.status !== 0) {
      fail(`bun build for ${pkg} failed (exit ${bun.status}).`);
    }
  }
  console.error("Host-side build complete.");
  console.error("");
}

function runGuestScript(): Promise<number> {
  // The guest script copies pre-built artifacts from the host share into a
  // local C:\aft tree, then runs tests/windows-e2e/run.ps1. All heavy
  // building happens on the Mac in buildArtifactsOnHost(); the VM only
  // executes runtime.
  console.error("\n--- guest output ---\n");

  const child = spawn(
    "prlctl",
    [
      "exec",
      VM_NAME,
      // Run as the logged-in interactive user, NOT the default prlctl-exec
      // identity (anonymous system token). The default token has its own
      // %USERPROFILE% that won't contain the user-installed Rust toolchain,
      // Bun, or npm globals. With --current-user, %USERPROFILE% resolves to
      // the actual logged-in user and `cargo`/`bun` on PATH light up after
      // the registry-PATH refresh in run-guest.ps1.
      "--current-user",
      "powershell.exe",
      "-NoProfile",
      "-ExecutionPolicy",
      "Bypass",
      "-File",
      // UNC path: drive-letter mappings don't carry across `prlctl exec`'s
      // session token, so Z:\ would resolve to "drive not found" here even
      // when the share is mounted. \\.psf\<share>\ works regardless.
      "\\\\.psf\\aft-repo\\scripts\\windows-vm\\run-guest.ps1",
    ],
    { stdio: ["ignore", "inherit", "inherit"] },
  );

  return new Promise<number>((resolve) => {
    child.on("close", (code) => resolve(code ?? -1));
  });
}

async function main() {
  preflight();

  // Build everything on the Mac BEFORE touching the VM. If the cross-compile
  // or Bun build fails, we want to know immediately, not after a 60-second
  // VM boot cycle. Building host-side also means subsequent runs benefit
  // from cargo + bun's own incremental caches on macOS, not the VM.
  buildArtifactsOnHost();

  // Stop the VM if it's currently running with a different state (we need
  // a clean snapshot revert).
  ensureVmStopped();

  revertSnapshot();
  startVm();
  await waitForGuestTools();

  ensureSharedFolder();

  const exitCode = await runGuestScript();

  console.error("\n--- guest finished ---");
  console.error(`Exit code: ${exitCode}`);

  // Suspend (not stop) so the next run starts fast. Don't suspend on
  // failure — leaves the VM available for ad-hoc PowerShell debugging.
  if (exitCode === 0) {
    console.error("Suspending VM (next run will resume from this state).");
    run("prlctl", ["suspend", VM_NAME], { silent: true });
  } else {
    console.error("Leaving VM running for debugging. To suspend manually: prlctl suspend " + VM_NAME);
  }

  process.exit(exitCode);
}

main().catch((err) => {
  console.error("Unhandled error:", err);
  process.exit(1);
});
