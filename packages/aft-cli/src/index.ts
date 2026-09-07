#!/usr/bin/env node
/**
 * @cortexkit/aft — unified CLI for Agent File Tools.
 *
 * Entry point parses argv and dispatches to commands. Harness selection
 * (OpenCode, Pi) is auto-detected from installed config paths; explicit
 * `--harness <name>` overrides detection.
 */

import { CLI } from "./lib/cli.js";

const command = process.argv[2];
const args = process.argv.slice(3);

function printHelp(): void {
  console.log("");
  console.log("  AFT CLI");
  console.log("  -------");
  console.log("");
  console.log("  Commands:");
  console.log("    --version        Show CLI, binary, and per-harness plugin versions");
  console.log("    setup            Interactive setup wizard");
  console.log("    index            Build one configured index snapshot (no scheduler)");
  console.log("    doctor           Check and fix configuration issues");
  console.log("    doctor --profile [seconds]  Profile a running AFT daemon");
  console.log("    doctor lsp <file> Inspect LSP setup for one file");
  console.log("    doctor --fix     Auto-fix common issues (e.g. ONNX Runtime mismatch)");
  console.log("    doctor --clear   Select caches to clear with an interactive prompt");
  console.log("    doctor --issue   Collect diagnostics and open a GitHub issue");
  console.log(
    "    doctor reset-build-breaker --root <root> --domain <domain> --fingerprint <fingerprint>",
  );
  console.log("");
  console.log("  Harness selection:");
  console.log("    --harness opencode    Target OpenCode only");
  console.log("    --harness pi          Target Pi only");
  console.log("    (default: auto-detect, prompt if multiple detected)");
  console.log("");
  console.log("  Usage:");
  console.log(`    ${CLI} setup`);
  console.log(`    ${CLI} index`);
  console.log(`    ${CLI} doctor`);
  console.log(`    ${CLI} doctor --profile 4`);
  console.log(`    ${CLI} doctor lsp ./src/main.py`);
  console.log(`    ${CLI} doctor --clear`);
  console.log(`    ${CLI} doctor --issue`);
  console.log(
    `    ${CLI} doctor reset-build-breaker --root <root> --domain <domain> --fingerprint <fingerprint>`,
  );
  console.log("");
}

async function main(): Promise<number> {
  if (command === "--version" || command === "-v" || command === "-V" || command === "version") {
    const { runVersion } = await import("./commands/version.js");
    return runVersion();
  }
  if (command === "setup") {
    const { runSetup } = await import("./commands/setup.js");
    return runSetup(args);
  }
  if (command === "index") {
    const { runIndex } = await import("./commands/index.js");
    return runIndex(args);
  }
  if (command === "doctor") {
    if (args.includes("--profile")) {
      const { runDoctorProfile } = await import("./commands/doctor.js");
      return runDoctorProfile(args);
    }
    if (args[0] === "lsp") {
      const { runLspDoctor } = await import("./commands/lsp.js");
      return runLspDoctor({ argv: args.slice(1) });
    }
    if (args[0] === "filters") {
      const { runDoctorFilters } = await import("./commands/doctor-filters.js");
      return runDoctorFilters({ argv: args.slice(1) });
    }
    if (args[0] === "reset-build-breaker") {
      const { runDoctorBuildBreakerReset } = await import("./commands/doctor.js");
      return runDoctorBuildBreakerReset(args.slice(1));
    }
    const { runDoctor } = await import("./commands/doctor.js");
    const force = args.includes("--force");
    const clear = args.includes("--clear");
    const fix = args.includes("--fix");
    const issue = args.includes("--issue");
    return runDoctor({ clear, fix, force, issue, argv: args });
  }
  printHelp();
  return command ? 1 : 0;
}

main().then((code) => process.exit(code));
