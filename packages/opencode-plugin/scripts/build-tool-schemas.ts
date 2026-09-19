#!/usr/bin/env bun
/**
 * Generates subc_tool_schemas.json for the agent-file-tools crate.
 *
 * Run: bun run build:tool-schemas
 * Output: crates/aft/src/subc_tool_schemas.json
 */

import * as path from "node:path";
import { buildSubcToolSchemasJson } from "../src/subc-tool-schemas.js";

async function main() {
  const pluginRoot = path.resolve(import.meta.dir, "..");
  const repoRoot = path.resolve(pluginRoot, "..", "..");
  const outputPath = path.join(repoRoot, "crates", "aft", "src", "subc_tool_schemas.json");
  const hashlineOutputPath = path.join(
    repoRoot,
    "crates",
    "aft",
    "src",
    "hashline_edit_schemas.json",
  );
  const checkOnly = process.argv.includes("--check");

  const json = buildSubcToolSchemasJson();
  if (checkOnly) {
    const existing = await Bun.file(outputPath).text();
    if (existing !== json) {
      throw new Error(
        `subc tool schema byte drift detected at ${outputPath}; run without --check to regenerate it`,
      );
    }
  } else {
    await Bun.write(outputPath, json);
  }

  // The generator is a tiny debug binary, so a compile cache buys nothing and
  // the sccache wrapper configured in .cargo/config.toml has wedged this step
  // twice (its client sleeps on the server socket, the check never returns).
  // Run it with the wrapper off and a bound so a wedge fails the check loudly
  // instead of parking the push.
  const generator = Bun.spawn(
    ["cargo", "run", "--quiet", "-p", "agent-file-tools", "--bin", "hashline-schema-artifact"],
    {
      cwd: repoRoot,
      stdout: "pipe",
      stderr: "inherit",
      env: { ...process.env, CARGO_BUILD_RUSTC_WRAPPER: "", RUSTC_WRAPPER: "" },
      timeout: 10 * 60 * 1000,
      killSignal: "SIGKILL",
    },
  );
  const hashlineJson = await new Response(generator.stdout).text();
  if ((await generator.exited) !== 0) {
    throw new Error(
      "governed hashline edit schema generator failed (or exceeded its 10-minute bound)",
    );
  }
  if (checkOnly) {
    const existing = await Bun.file(hashlineOutputPath).text();
    if (existing !== hashlineJson) {
      throw new Error(
        `hashline edit schema byte drift detected at ${hashlineOutputPath}; run without --check to regenerate it`,
      );
    }
  } else {
    await Bun.write(hashlineOutputPath, hashlineJson);
  }

  const count = Object.keys(JSON.parse(json) as Record<string, unknown>).length;
  console.log(
    `✓ subc tool schemas (${count} tools) ${checkOnly ? "match" : "written"}: ${outputPath}`,
  );
  console.log(
    `✓ governed hashline edit schemas ${checkOnly ? "match" : "written"}: ${hashlineOutputPath}`,
  );
}

main();
