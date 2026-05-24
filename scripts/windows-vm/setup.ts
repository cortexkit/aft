#!/usr/bin/env bun
/**
 * One-time orchestrator for setting up the Windows VM as a test target.
 *
 * Assumes the user already has Parallels Desktop installed and has manually
 * created a Windows 11 ARM64 VM (named "Windows 11" by default — override
 * with AFT_WINDOWS_VM_NAME). Why "manual" — Windows installation requires
 * an interactive ISO boot that can't reasonably be automated from CLI in a
 * way that benefits us; once the OS is installed, everything else IS
 * automatable.
 *
 * What this script does:
 *   1. Verifies prlctl + the named VM exist.
 *   2. Mounts the repo as a shared folder so setup-guest.ps1 is reachable.
 *   3. Resumes/starts the VM.
 *   4. Prints clear instructions for running setup-guest.ps1 inside the VM
 *      and then taking the "aft-ready" snapshot. We don't try to run the
 *      guest setup automatically because:
 *        - The first invocation needs Administrator rights (UAC prompt).
 *        - User confirmation for winget/npm install dialogs is real.
 *        - Errors during install are easier to debug interactively.
 *      After the user signals completion, we take the snapshot.
 *
 * Subsequent test cycles use `bun run test:windows-e2e`, which is fully
 * automated end-to-end.
 */

import { spawnSync } from "node:child_process";
import { resolve } from "node:path";

// Match the default in scripts/windows-vm/test.ts. Override via env var
// when your VM has a different Parallels name.
const VM_NAME = process.env.AFT_WINDOWS_VM_NAME ?? "AFT Windows";
const SNAPSHOT_NAME = process.env.AFT_WINDOWS_SNAPSHOT ?? "aft-ready";
const REPO_ROOT = resolve(import.meta.dirname, "..", "..");
const SHARE_NAME = "aft-repo";

interface CmdResult {
  status: number;
  stdout: string;
  stderr: string;
}

function run(cmd: string, args: string[]): CmdResult {
  console.error(`$ ${cmd} ${args.join(" ")}`);
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
  const v = run("prlctl", ["--version"]);
  if (v.status !== 0) {
    fail(
      "prlctl not found on PATH.",
      "Install Parallels Desktop: https://www.parallels.com/products/desktop/",
    );
  }

  const list = run("prlctl", ["list", "-a", "--no-header", "-o", "name"]);
  const vmExists = list.stdout.split(/\r?\n/).some((line) => line.trim() === VM_NAME);
  if (!vmExists) {
    fail(
      `VM "${VM_NAME}" not found in Parallels.`,
      "Create a Windows 11 ARM64 VM in Parallels Desktop, then re-run this script. " +
        "Override the name via AFT_WINDOWS_VM_NAME if you used a different one.",
    );
  }

  // Snapshot already exists?
  //
  // Note: plain `prlctl snapshot-list <vm>` returns only IDs (no names), so a
  // substring match on its stdout for SNAPSHOT_NAME would never hit. We use
  // --json to get a {<id>: {name, date, ...}} map and check the names.
  const snaps = run("prlctl", ["snapshot-list", VM_NAME, "--json"]);
  let existingNames: string[] = [];
  try {
    const parsed = JSON.parse(snaps.stdout) as Record<string, { name: string }>;
    existingNames = Object.values(parsed).map((s) => s.name);
  } catch {
    // If parsing fails (older prlctl, no snapshots, etc.) treat as no match
    // and proceed; takeSnapshot will fail loudly if there's a real conflict.
    existingNames = [];
  }
  if (existingNames.includes(SNAPSHOT_NAME)) {
    console.error(
      `Snapshot "${SNAPSHOT_NAME}" already exists. ` +
        "Delete it first if you want to recreate it.",
    );
    process.exit(0);
  }
}

function ensureSharedFolder() {
  console.error(`Adding shared folder "${SHARE_NAME}" -> ${REPO_ROOT}...`);
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

function startVm() {
  const status = run("prlctl", ["list", "-a", "--no-header", "-o", "status,name"]);
  const isStopped = !status.stdout
    .split(/\r?\n/)
    .some((line) => line.includes(VM_NAME) && line.startsWith("running"));
  if (isStopped) {
    console.error(`Starting VM "${VM_NAME}"...`);
    const r = run("prlctl", ["start", VM_NAME]);
    if (r.status !== 0 && !r.stderr.toLowerCase().includes("already running")) {
      fail(`prlctl start failed: ${r.stderr.trim()}`);
    }
  } else {
    console.error(`VM "${VM_NAME}" is already running.`);
  }
}

function waitForUserToFinishGuestSetup() {
  console.error("");
  console.error("============================================================");
  console.error("  GUEST SETUP -- RUN THIS INSIDE THE VM");
  console.error("============================================================");
  console.error("");
  console.error("  1. Open Start -> search for 'PowerShell' -> right-click ->");
  console.error("     'Run as administrator'. Accept the UAC prompt.");
  console.error("");
  console.error("  2. In the elevated PowerShell window, run this single line:");
  console.error("");
  console.error(
    "       powershell -ExecutionPolicy Bypass -File \\\\.psf\\aft-repo\\scripts\\windows-vm\\setup-guest.ps1",
  );
  console.error("");
  console.error("     Why \\\\.psf\\... and not Z:\\... -- mapped drive letters");
  console.error("     don't carry across UAC token boundaries, so Z: is invisible");
  console.error("     to elevated PowerShell. The UNC path \\\\.psf\\aft-repo\\");
  console.error("     reaches the same shared folder regardless of token.");
  console.error("");
  console.error("  3. The script installs Bun, Rust, Node.js, OpenCode, aimock,");
  console.error("     pwsh, and configures Defender exclusions. 5-10 minutes.");
  console.error("");
  console.error("  4. When it prints 'Setup complete', come back here and");
  console.error("     press Enter to take the 'aft-ready' snapshot.");
  console.error("");
  console.error("  Press Enter when guest setup is finished, or Ctrl-C to abort.");

  // Block until Enter. Using spawnSync to get a synchronous read.
  spawnSync("bash", ["-c", "read"], { stdio: "inherit" });
}

function takeSnapshot() {
  console.error(`Taking snapshot "${SNAPSHOT_NAME}"...`);
  const r = run("prlctl", ["snapshot", VM_NAME, "--name", SNAPSHOT_NAME]);
  if (r.status !== 0) {
    fail(`prlctl snapshot failed: ${r.stderr.trim() || r.stdout.trim()}`);
  }
}

function suspendVm() {
  console.error(`Suspending VM "${VM_NAME}"...`);
  run("prlctl", ["suspend", VM_NAME]);
}

function main() {
  preflight();
  ensureSharedFolder();
  startVm();
  waitForUserToFinishGuestSetup();
  takeSnapshot();
  suspendVm();

  console.error("");
  console.error("============================================");
  console.error(`  Setup complete. Snapshot "${SNAPSHOT_NAME}" created.`);
  console.error("============================================");
  console.error("");
  console.error("Run tests with:");
  console.error("  bun run test:windows-e2e");
}

main();
