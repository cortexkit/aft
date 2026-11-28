#!/usr/bin/env bun
/**
 * Generates the daemon tool schema and preset artifacts for the aft-subc crate.
 *
 * Run: bun run build:tool-schemas
 * Output: crates/aft-subc/src/subc_tool_*.json
 */

import * as path from "node:path";
import {
  buildSubcRemoteToolSchemasJson,
  buildSubcRevisionToolSchemasJson,
  buildSubcToolPresetsJson,
  buildSubcToolSchemasJson,
} from "../src/subc-tool-schemas.js";

async function main() {
  const pluginRoot = path.resolve(import.meta.dir, "..");
  const repoRoot = path.resolve(pluginRoot, "..", "..");
  const subcAssetDir = path.join(repoRoot, "crates", "aft-subc", "src");
  const outputPath = path.join(subcAssetDir, "subc_tool_schemas.json");
  const hashlineOutputPath = path.join(
    repoRoot,
    "crates",
    "aft-engine",
    "src",
    "hashline_edit_schemas.json",
  );
  const sourceOnly = process.argv.includes("--check-source-only");
  const checkOnly = process.argv.includes("--check") || sourceOnly;

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

  const presetsPath = path.join(subcAssetDir, "subc_tool_presets.json");
  const presetsJson = buildSubcToolPresetsJson();
  if (checkOnly) {
    const existing = await Bun.file(presetsPath).text();
    if (existing !== presetsJson) {
      throw new Error(
        `subc tool preset byte drift detected at ${presetsPath}; run without --check to regenerate it`,
      );
    }
  } else {
    await Bun.write(presetsPath, presetsJson);
  }

  const remotePath = path.join(subcAssetDir, "subc_tool_remote_schemas.json");
  const remoteJson = buildSubcRemoteToolSchemasJson();
  if (checkOnly) {
    if ((await Bun.file(remotePath).text()) !== remoteJson) {
      throw new Error(`subc remote tool schema byte drift detected at ${remotePath}`);
    }
  } else {
    await Bun.write(remotePath, remoteJson);
  }

  const revisionPath = path.join(subcAssetDir, "subc_tool_revision_schemas.json");
  const revisionJson = buildSubcRevisionToolSchemasJson();
  if (checkOnly) {
    if ((await Bun.file(revisionPath).text()) !== revisionJson) {
      throw new Error(`subc revision-read tool schema byte drift detected at ${revisionPath}`);
    }
  } else {
    await Bun.write(revisionPath, revisionJson);
  }

  // --check-source-only avoids compiling the native library before a train
  // push. The unit CI workflow and release gates still run the full --check.
  if (!sourceOnly) {
    // CI supplies the generator from its preceding Cargo build. If it is
    // missing, fail rather than silently compiling a different feature set.
    const prebuilt = process.env.AFT_HASHLINE_SCHEMA_GENERATOR;
    const command =
      prebuilt === undefined
        ? ["cargo", "run", "--quiet", "-p", "agent-file-tools", "--bin", "hashline-schema-artifact"]
        : [path.resolve(repoRoot, prebuilt)];
    const generator = Bun.spawn(command, {
      cwd: repoRoot,
      stdout: "pipe",
      stderr: "inherit",
      // The compiler cache wrapper has hung on its socket during local Cargo builds.
      env: { ...process.env, CARGO_BUILD_RUSTC_WRAPPER: "", RUSTC_WRAPPER: "" },
      timeout: 10 * 60 * 1000,
      killSignal: "SIGKILL",
    });
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
  }

  const count = Object.keys(JSON.parse(json) as Record<string, unknown>).length;
  console.log(
    `✓ subc tool schemas (${count} tools) ${checkOnly ? "match" : "written"}: ${outputPath}`,
  );
  console.log(`✓ enabled-only subc bash schemas ${checkOnly ? "match" : "written"}: ${remotePath}`);
  console.log(
    `✓ enabled-only subc revision-read schemas ${checkOnly ? "match" : "written"}: ${revisionPath}`,
  );
  console.log(
    sourceOnly
      ? "→ governed hashline byte comparison deferred to the native CI/release gate"
      : `✓ governed hashline edit schemas ${checkOnly ? "match" : "written"}: ${hashlineOutputPath}`,
  );
}

main();
