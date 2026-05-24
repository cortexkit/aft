#!/usr/bin/env node
/**
 * @cortexkit/aft — unified CLI for Agent File Tools.
 *
 * Entry point parses argv and dispatches to commands. Harness selection
 * (OpenCode, Pi) is auto-detected from installed config paths; explicit
 * `--harness <name>` overrides detection.
 */

const command = process.argv[2];
const args = process.argv.slice(3);

function printHelp(): void {
  console.log("");
  console.log("  AFT CLI");
  console.log("  -------");
  console.log("");
  console.log("  Commands:");
  console.log("    setup            Interactive setup wizard");
  console.log("    doctor           Check and fix configuration issues");
  console.log("    doctor lsp <file> Inspect LSP setup for one file");
  console.log("    doctor --fix     Auto-fix common issues (e.g. ONNX Runtime mismatch)");
  console.log("    doctor --clear   Select caches to clear with an interactive prompt");
  console.log("    doctor --issue   Collect diagnostics and open a GitHub issue");
  console.log("");
  console.log("  Harness selection:");
  console.log("    --harness opencode    Target OpenCode only");
  console.log("    --harness pi          Target Pi only");
  console.log("    (default: auto-detect, prompt if multiple detected)");
  console.log("");
  console.log("  Usage:");
  console.log("    npx @cortexkit/aft setup");
  console.log("    npx @cortexkit/aft doctor");
  console.log("    npx @cortexkit/aft doctor lsp ./src/main.py");
  console.log("    npx @cortexkit/aft doctor --clear");
  console.log("    npx @cortexkit/aft doctor --issue");
  console.log("");
}

async function main(): Promise<number> {
  if (command === "setup") {
    const { runSetup } = await import("./commands/setup.js");
    return runSetup(args);
  }
  if (command === "doctor") {
    if (args[0] === "lsp") {
      const { runLspDoctor } = await import("./commands/lsp.js");
      return runLspDoctor({ argv: args.slice(1) });
    }
    if (args[0] === "filters") {
      const { runDoctorFilters } = await import("./commands/doctor-filters.js");
      return runDoctorFilters({ argv: args.slice(1) });
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
