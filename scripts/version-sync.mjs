#!/usr/bin/env node

/**
 * version-sync.mjs
 *
 * Synchronizes version across all AFT packages from a single source of truth.
 *
 * Usage:
 *   node scripts/version-sync.mjs 0.2.0           # set version to 0.2.0
 *   node scripts/version-sync.mjs --from-tag       # read from GITHUB_REF_NAME (e.g. v0.2.0)
 *   node scripts/version-sync.mjs 0.2.0 --dry-run  # preview changes without writing
 *
 * Updates 10 locations:
 *   1-5. npm/{platform}/package.json  → version field
 *   6.   aft-bridge/package.json → version field
 *   7.   aft-opencode/package.json → version field + all optionalDependencies versions + aft-bridge dep
 *   8.   aft-pi/package.json → version field + all optionalDependencies versions + aft-bridge dep
 *   9.   aft-cli/package.json → version field + aft-bridge dep (workspace:* → semver)
 *   10.  Cargo.toml → version field
 */

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = join(__dirname, "..");

const SEMVER_RE = /^\d+\.\d+\.\d+(?:-[\w.]+)?(?:\+[\w.]+)?$/;

const PLATFORM_DIRS = [
  "darwin-arm64",
  "darwin-x64",
  "linux-arm64",
  "linux-x64",
  "win32-arm64",
  "win32-x64",
];

function parseArgs(argv) {
  const args = argv.slice(2);
  let version = null;
  let fromTag = false;
  let dryRun = false;

  for (const arg of args) {
    if (arg === "--from-tag") {
      fromTag = true;
    } else if (arg === "--dry-run") {
      dryRun = true;
    } else if (!version && !arg.startsWith("-")) {
      version = arg;
    } else {
      console.error(`Unknown argument: ${arg}`);
      process.exit(1);
    }
  }

  if (fromTag) {
    const ref = process.env.GITHUB_REF_NAME;
    if (!ref) {
      console.error("--from-tag requires GITHUB_REF_NAME environment variable");
      process.exit(1);
    }
    // Strip leading 'v' from tag (e.g. v0.2.0 → 0.2.0)
    version = ref.replace(/^v/, "");
  }

  if (!version) {
    console.error(
      "Usage: version-sync.mjs <version> [--dry-run]\n" +
        "       version-sync.mjs --from-tag [--dry-run]",
    );
    process.exit(1);
  }

  if (!SEMVER_RE.test(version)) {
    console.error(`Invalid semver version: '${version}'`);
    process.exit(1);
  }

  return { version, dryRun };
}

function updateJsonFile(filePath, version, updates, dryRun) {
  const content = readFileSync(filePath, "utf-8");
  const pkg = JSON.parse(content);
  const changes = [];

  if (pkg.version !== version) {
    changes.push(`  version: ${pkg.version} → ${version}`);
    pkg.version = version;
  }

  // Update optionalDependencies versions if requested
  if (updates?.optionalDependencies && pkg.optionalDependencies) {
    for (const [dep, oldVer] of Object.entries(pkg.optionalDependencies)) {
      if (oldVer !== version) {
        changes.push(`  optionalDependencies["${dep}"]: ${oldVer} → ${version}`);
        pkg.optionalDependencies[dep] = version;
      }
    }
  }

  // Update internal @cortexkit/aft-* dependencies (e.g. aft-bridge in plugins)
  if (updates?.internalDeps && pkg.dependencies) {
    for (const [dep, oldVer] of Object.entries(pkg.dependencies)) {
      if (dep.startsWith("@cortexkit/aft-") && oldVer !== version) {
        changes.push(`  dependencies["${dep}"]: ${oldVer} → ${version}`);
        pkg.dependencies[dep] = version;
      }
    }
  }

  if (changes.length === 0) {
    return { path: filePath, changes: ["  (already at target version)"] };
  }

  if (!dryRun) {
    writeFileSync(filePath, `${JSON.stringify(pkg, null, 2)}\n`, "utf-8");
  }

  return { path: filePath, changes };
}

function updateCargoToml(filePath, version, dryRun) {
  const content = readFileSync(filePath, "utf-8");
  const changes = [];

  // Match the version line under [package] — first version = line in [package] section
  const versionRe = /^(version\s*=\s*)"([^"]+)"/m;
  const match = content.match(versionRe);

  if (!match) {
    return { path: filePath, changes: ["  WARNING: could not find version field"] };
  }

  if (match[2] === version) {
    return { path: filePath, changes: ["  (already at target version)"] };
  }

  changes.push(`  version: ${match[2]} → ${version}`);

  if (!dryRun) {
    const updated = content.replace(versionRe, `$1"${version}"`);
    writeFileSync(filePath, updated, "utf-8");
  }

  return { path: filePath, changes };
}

/**
 * Update an inline path-dep version pin in a Cargo.toml.
 *
 * Matches a line of the form:
 *   <depName> = { path = "...", version = "<old>" [, ...] }
 *
 * and rewrites just the `version = "..."` segment. The path remains
 * load-bearing for workspace builds; the version is required so cargo
 * accepts the manifest for publication and so consumers resolving from
 * crates.io get the matching version.
 */
function updateCargoPathDep(filePath, depName, version, dryRun) {
  const content = readFileSync(filePath, "utf-8");
  // depName can contain hyphens — escape for safe regex insertion.
  const depEscaped = depName.replace(/[-/\\^$*+?.()|[\]{}]/g, "\\$&");
  // Match a line starting with `<dep> = { ... version = "<old>" ... }`.
  // Capture the prefix up to (and including) `version = "` so we can rewrite
  // just the version literal without touching path or other fields.
  const re = new RegExp(`^(${depEscaped}\\s*=\\s*\\{[^\\n]*version\\s*=\\s*)"([^"]+)"`, "m");
  const match = content.match(re);

  if (!match) {
    return {
      path: filePath,
      changes: [`  WARNING: could not find ${depName} path-dep with version pin`],
    };
  }

  if (match[2] === version) {
    return { path: filePath, changes: [`  (${depName} dep already at target version)`] };
  }

  const changes = [`  ${depName} dep version: ${match[2]} → ${version}`];

  if (!dryRun) {
    const updated = content.replace(re, `$1"${version}"`);
    writeFileSync(filePath, updated, "utf-8");
  }

  return { path: filePath, changes };
}

// --- Main ---

const { version, dryRun } = parseArgs(process.argv);

console.log(`${dryRun ? "[DRY RUN] " : ""}Syncing version to ${version}\n`);

const results = [];

// 1-5: Platform packages
for (const dir of PLATFORM_DIRS) {
  const filePath = join(root, "packages", "npm", dir, "package.json");
  results.push(updateJsonFile(filePath, version, {}, dryRun));
}

// 6: @cortexkit/aft-bridge (shared transport)
const bridgePath = join(root, "packages", "aft-bridge", "package.json");
results.push(updateJsonFile(bridgePath, version, {}, dryRun));

// 7: @cortexkit/aft-opencode
const corePath = join(root, "packages", "opencode-plugin", "package.json");
results.push(
  updateJsonFile(corePath, version, { optionalDependencies: true, internalDeps: true }, dryRun),
);

// 8: @cortexkit/aft-pi
const piPath = join(root, "packages", "pi-plugin", "package.json");
results.push(
  updateJsonFile(piPath, version, { optionalDependencies: true, internalDeps: true }, dryRun),
);

// 9: @cortexkit/aft (unified CLI)
// internalDeps: true rewrites `@cortexkit/aft-bridge: "workspace:*"` → the
// real semver at publish time. Bun resolves workspace:* in local dev but
// `npm publish` does not, so without this rewrite the published tarball
// leaks the protocol literally and `npx @cortexkit/aft@<version>` fails
// with EUNSUPPORTEDPROTOCOL on install. See note for v0.28.0 → v0.28.1
// regression: aft-bridge wasn't a runtime dep of the CLI before #46 added
// doctor --fix's ensureBinary() call, so this gap stayed latent until that
// commit shipped.
const cliPath = join(root, "packages", "aft-cli", "package.json");
results.push(updateJsonFile(cliPath, version, { internalDeps: true }, dryRun));

// 10: Cargo.toml — agent-file-tools (main crate)
const cargoPath = join(root, "crates", "aft", "Cargo.toml");
results.push(updateCargoToml(cargoPath, version, dryRun));

// 11: Cargo.toml — aft-tokenizer (leaf crate, published first so the main
// crate can resolve its `version =` constraint from crates.io). Also keep
// the inline path-dep version pin (`aft-tokenizer = { path = "...",
// version = "<version>" }`) in `crates/aft/Cargo.toml` in lockstep — cargo
// refuses to publish a crate whose path-dep has no `version =`.
const tokenizerPath = join(root, "crates", "aft-tokenizer", "Cargo.toml");
results.push(updateCargoToml(tokenizerPath, version, dryRun));
results.push(updateCargoPathDep(cargoPath, "aft-tokenizer", version, dryRun));

// Report
let updateCount = 0;
for (const { path, changes } of results) {
  const relativePath = path.replace(`${root}/`, "");
  console.log(`${relativePath}:`);
  for (const change of changes) {
    console.log(change);
    if (!change.includes("already at")) updateCount++;
  }
}

console.log(
  `\n${dryRun ? "[DRY RUN] " : ""}${updateCount} update(s) across ${results.length} files.`,
);
