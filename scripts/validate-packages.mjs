#!/usr/bin/env node

/**
 * validate-packages.mjs
 *
 * Validates all 9 AFT npm package.json files:
 * - 5 platform packages under npm/{platform}/
 * - @cortexkit/aft-bridge (aft-bridge — shared transport)
 * - @cortexkit/aft-opencode (opencode-plugin)
 * - @cortexkit/aft-pi (pi-plugin)
 * - @cortexkit/aft (aft-cli)
 *
 * Checks: os/cpu fields match directory, preferUnplugged, bin field,
 * optionalDependencies in core, plugins' aft-bridge dep version,
 * version alignment, required fields.
 *
 * Exit 0 = all pass. Exit 1 = failures printed to stderr.
 */

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = join(__dirname, "..");

const PLATFORMS = [
  { dir: "darwin-arm64", os: "darwin", cpu: "arm64" },
  { dir: "darwin-x64", os: "darwin", cpu: "x64" },
  { dir: "linux-arm64", os: "linux", cpu: "arm64" },
  { dir: "linux-x64", os: "linux", cpu: "x64" },
  { dir: "win32-arm64", os: "win32", cpu: "arm64" },
  { dir: "win32-x64", os: "win32", cpu: "x64" },
];

const errors = [];

function fail(pkg, msg) {
  errors.push(`[${pkg}] ${msg}`);
}

function readPkg(path) {
  try {
    return JSON.parse(readFileSync(path, "utf-8"));
  } catch (e) {
    errors.push(`Cannot read ${path}: ${e.message}`);
    return null;
  }
}

// --- Validate platform packages ---

const platformVersions = [];

for (const { dir, os, cpu } of PLATFORMS) {
  const pkgPath = join(root, "packages", "npm", dir, "package.json");
  const pkg = readPkg(pkgPath);
  if (!pkg) continue;

  const label = `@cortexkit/aft-${dir}`;

  // Required fields
  if (!pkg.name) fail(label, "missing 'name'");
  if (!pkg.version) fail(label, "missing 'version'");
  if (pkg.name && pkg.name !== `@cortexkit/aft-${dir}`) {
    fail(label, `name should be '@cortexkit/aft-${dir}', got '${pkg.name}'`);
  }

  // os/cpu arrays
  if (!Array.isArray(pkg.os) || pkg.os.length !== 1 || pkg.os[0] !== os) {
    fail(label, `os should be ["${os}"], got ${JSON.stringify(pkg.os)}`);
  }
  if (!Array.isArray(pkg.cpu) || pkg.cpu.length !== 1 || pkg.cpu[0] !== cpu) {
    fail(label, `cpu should be ["${cpu}"], got ${JSON.stringify(pkg.cpu)}`);
  }

  // preferUnplugged
  if (pkg.preferUnplugged !== true) {
    fail(label, "missing or false 'preferUnplugged: true'");
  }

  // bin field
  if (!pkg.bin || typeof pkg.bin !== "object") {
    fail(label, "missing 'bin' field");
  } else if (!pkg.bin.aft) {
    fail(label, "bin field missing 'aft' entry");
  }

  if (pkg.version) platformVersions.push({ name: label, version: pkg.version });
}

// --- Validate @cortexkit/aft-bridge ---

const bridgePath = join(root, "packages", "aft-bridge", "package.json");
const bridge = readPkg(bridgePath);

if (bridge) {
  const label = "@cortexkit/aft-bridge";

  if (!bridge.name) fail(label, "missing 'name'");
  if (!bridge.version) fail(label, "missing 'version'");
  if (bridge.name && bridge.name !== "@cortexkit/aft-bridge") {
    fail(label, `name should be '@cortexkit/aft-bridge', got '${bridge.name}'`);
  }
  if (!bridge.main) fail(label, "missing 'main'");
  if (!bridge.types) fail(label, "missing 'types'");
  if (!bridge.license) fail(label, "missing 'license'");
  if (!bridge.repository || typeof bridge.repository !== "object" || !bridge.repository.url) {
    fail(label, "missing 'repository' with 'url'");
  }
  if (bridge.bin) {
    fail(label, "must not declare 'bin' (library package, not a CLI)");
  }

  if (bridge.version) platformVersions.push({ name: label, version: bridge.version });
}

// --- Validate @cortexkit/aft-opencode ---

const corePath = join(root, "packages", "opencode-plugin", "package.json");
const core = readPkg(corePath);

if (core) {
  const label = "@cortexkit/aft-opencode";

  // Required fields
  if (!core.name) fail(label, "missing 'name'");
  if (!core.version) fail(label, "missing 'version'");
  if (core.name && core.name !== "@cortexkit/aft-opencode") {
    fail(label, `name should be '@cortexkit/aft-opencode', got '${core.name}'`);
  }

  // optionalDependencies must list all 5 platform packages
  const optDeps = core.optionalDependencies || {};
  for (const { dir } of PLATFORMS) {
    const depName = `@cortexkit/aft-${dir}`;
    if (!(depName in optDeps)) {
      fail(label, `optionalDependencies missing '${depName}'`);
    }
  }

  const coreFiles = Array.isArray(core.files) ? core.files : [];
  if (!coreFiles.includes("src/tui-compiled")) {
    fail(
      label,
      "files must include 'src/tui-compiled' so the published TUI ships its precompiled tree",
    );
  }

  // Both OpenCode generations resolve `<pkg>/tui` through the exports map.
  // The entry under src/entry/ wraps the precompiled TUI in the V2 loader's
  // {id, tui, setup} shape and re-exports the V1 fields, so one path serves
  // both hosts. It must be published alongside the tree it wraps.
  const tuiExportImport = core.exports?.["./tui"]?.import;
  if (tuiExportImport !== "./src/entry/tui.mjs") {
    fail(
      label,
      `exports['./tui'].import must be './src/entry/tui.mjs', got '${tuiExportImport ?? "undefined"}'`,
    );
  }
  for (const dir of ["src/entry", "src/tui"]) {
    if (!coreFiles.includes(dir)) {
      fail(
        label,
        `files must include '${dir}' so the ./tui export resolves in the published package`,
      );
    }
  }

  if (core.version) platformVersions.push({ name: label, version: core.version });
}

// --- Validate @cortexkit/aft-pi ---

const piPath = join(root, "packages", "pi-plugin", "package.json");
const pi = readPkg(piPath);

if (pi) {
  const label = "@cortexkit/aft-pi";

  // Required fields
  if (!pi.name) fail(label, "missing 'name'");
  if (!pi.version) fail(label, "missing 'version'");
  if (pi.name && pi.name !== "@cortexkit/aft-pi") {
    fail(label, `name should be '@cortexkit/aft-pi', got '${pi.name}'`);
  }
  if (!pi.main) fail(label, "missing 'main'");
  if (!pi.types) fail(label, "missing 'types'");
  if (!pi.license) fail(label, "missing 'license'");
  if (!pi.repository || typeof pi.repository !== "object" || !pi.repository.url) {
    fail(label, "missing 'repository' with 'url'");
  }

  // optionalDependencies must list all 5 platform packages
  const piOptDeps = pi.optionalDependencies || {};
  for (const { dir } of PLATFORMS) {
    const depName = `@cortexkit/aft-${dir}`;
    if (!(depName in piOptDeps)) {
      fail(label, `optionalDependencies missing '${depName}'`);
    }
  }

  if (pi.version) platformVersions.push({ name: label, version: pi.version });
}

// --- Validate @cortexkit/aft (CLI) ---

const cliPath = join(root, "packages", "aft-cli", "package.json");
const cli = readPkg(cliPath);

if (cli) {
  const label = "@cortexkit/aft";

  // Required fields
  if (!cli.name) fail(label, "missing 'name'");
  if (!cli.version) fail(label, "missing 'version'");
  if (cli.name && cli.name !== "@cortexkit/aft") {
    fail(label, `name should be '@cortexkit/aft', got '${cli.name}'`);
  }
  if (!cli.license) fail(label, "missing 'license'");
  if (!cli.repository || typeof cli.repository !== "object" || !cli.repository.url) {
    fail(label, "missing 'repository' with 'url'");
  }

  // bin field
  if (!cli.bin || typeof cli.bin !== "object") {
    fail(label, "missing 'bin' field");
  } else if (!cli.bin.aft) {
    fail(label, "bin field missing 'aft' entry");
  }

  if (cli.version) platformVersions.push({ name: label, version: cli.version });
}

// --- Publish-safety: no unpublishable protocols in published `dependencies` ---
//
// Published packages must never carry a `file:`/`workspace:`/`link:` dependency
// in their runtime `dependencies` — npm consumers would fail to install it.
//
// @cortexkit/subc-client history: while it was an UNPUBLISHED path dep it had to
// stay a devDependency (bundled into plugin dists). Now that it is published to
// npm, aft-bridge declares it as a regular runtime dependency — REQUIRED because
// aft-bridge's own published dist is built with tsc (no bundling): dist/
// subc-transport.js carries a bare `from "@cortexkit/subc-client"` import that
// consumers resolving the published aft-bridge (e.g. the raw-TSX TUI sidebar
// under the OpenCode host) must be able to install. Shipping it as a devDep
// broke the v0.43.0 TUI sidebar with a silent MODULE_NOT_FOUND.
for (const { label, pkg } of [
  { label: "@cortexkit/aft-bridge", pkg: bridge },
  { label: "@cortexkit/aft-opencode", pkg: core },
  { label: "@cortexkit/aft-pi", pkg: pi },
  { label: "@cortexkit/aft", pkg: cli },
]) {
  if (!pkg) continue;
  const deps = pkg.dependencies || {};
  for (const [depName, depSpec] of Object.entries(deps)) {
    if (typeof depSpec === "string" && /^(file:|workspace:|link:)/.test(depSpec)) {
      fail(
        label,
        `dependencies['${depName}'] uses unpublishable protocol '${depSpec}' (move to devDependencies + bundle)`,
      );
    }
  }
}

// aft-bridge's published dist REQUIRES subc-client at runtime (bare import in
// dist/subc-transport.js — tsc build, nothing inlined). Guard the dependency
// direction so it can never silently regress to a devDep again.
{
  const deps = bridge?.dependencies || {};
  if (!deps["@cortexkit/subc-client"]) {
    fail(
      "@cortexkit/aft-bridge",
      "@cortexkit/subc-client must be a runtime dependency: dist/subc-transport.js imports it bare (tsc build, not bundled) and consumers of the published package fail to resolve it otherwise",
    );
  }
  if (
    deps["@cortexkit/subc-client"] &&
    /^(file:|workspace:|link:)/.test(deps["@cortexkit/subc-client"])
  ) {
    fail(
      "@cortexkit/aft-bridge",
      "@cortexkit/subc-client dependency must reference the published npm version, never a local path",
    );
  }
}

// --- Version alignment ---

if (platformVersions.length > 1) {
  const first = platformVersions[0];
  for (let i = 1; i < platformVersions.length; i++) {
    const other = platformVersions[i];
    if (other.version !== first.version) {
      fail("version-alignment", `${first.name}@${first.version} != ${other.name}@${other.version}`);
    }
  }
}

// Also check that optionalDependencies versions match the core version
if (core?.version && core.optionalDependencies) {
  for (const [depName, depVersion] of Object.entries(core.optionalDependencies)) {
    if (depVersion !== core.version) {
      fail(
        "version-alignment",
        `@cortexkit/aft-opencode optionalDependencies['${depName}'] is '${depVersion}' but core version is '${core.version}'`,
      );
    }
  }
}

// Also check that optionalDependencies versions match the pi version
if (pi?.version && pi.optionalDependencies) {
  for (const [depName, depVersion] of Object.entries(pi.optionalDependencies)) {
    if (depVersion !== pi.version) {
      fail(
        "version-alignment",
        `@cortexkit/aft-pi optionalDependencies['${depName}'] is '${depVersion}' but pi version is '${pi.version}'`,
      );
    }
  }
}

// Plugins and the unified CLI must depend on @cortexkit/aft-bridge at the
// matching version. Without this check, version-sync drift would let a package
// publish with a stale bridge dep that doesn't exist on npm yet (the failure
// mode that motivated wiring).
function checkBridgeDep(label, pkg) {
  if (!pkg?.dependencies || !pkg.version) return;
  const bridgeDep = pkg.dependencies["@cortexkit/aft-bridge"];
  if (!bridgeDep) {
    fail(label, "dependencies missing '@cortexkit/aft-bridge'");
    return;
  }
  if (bridgeDep !== pkg.version) {
    fail(
      "version-alignment",
      `${label} dependencies['@cortexkit/aft-bridge'] is '${bridgeDep}' but ${label} version is '${pkg.version}'`,
    );
  }
}
checkBridgeDep("@cortexkit/aft-opencode", core);
checkBridgeDep("@cortexkit/aft-pi", pi);
checkBridgeDep("@cortexkit/aft", cli);

// --- Report ---

if (errors.length > 0) {
  console.error("Package validation FAILED:\n");
  for (const err of errors) {
    console.error(`  ✗ ${err}`);
  }
  console.error(`\n${errors.length} error(s) found.`);
  process.exit(1);
} else {
  const count = PLATFORMS.length + 4; // platform packages + bridge + opencode + pi + cli
  console.log(`✓ All ${count} packages validated successfully.`);
  console.log(`  Versions aligned at ${platformVersions[0]?.version || "unknown"}`);
  console.log("  Platform os/cpu fields correct");
  console.log("  preferUnplugged set on all platform packages");
  console.log("  optionalDependencies complete in @cortexkit/aft-opencode and @cortexkit/aft-pi");
  console.log(
    "  @cortexkit/aft-opencode ships src/tui-compiled and exports ./tui through entry.mjs",
  );
  console.log("  @cortexkit/aft-bridge dep version aligned in plugin and CLI packages");
  console.log("  bin, license, repository fields present in @cortexkit/aft and @cortexkit/aft-pi");
  process.exit(0);
}
