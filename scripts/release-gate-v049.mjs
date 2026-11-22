#!/usr/bin/env node
/**
 * Validate the v0.49 artifact and activation release gates.
 *
 * The gate treats the prepared publication manifest as a byte contract for
 * the planned release. It may be built from a later commit, but its governed
 * source bytes must still be the exact bytes recorded at the candidate commit.
 * This prevents a release build from quietly accepting a steering or schema
 * edit that was never committed as part of the candidate.
 *
 * Usage (run each line separately; prepare before validating saved records):
 *   node scripts/release-gate-v049.mjs --check-inputs
 *   node scripts/release-gate-v049.mjs --prepare --snapshot-dir /tmp/release-snapshot --candidate
 *   node scripts/release-gate-v049.mjs --snapshot-dir /tmp/release-snapshot --candidate --require-platform-artifacts
 */

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, isAbsolute, join, relative, resolve, sep } from "node:path";

const ROOT = resolve(import.meta.dirname, "..");
const RELEASE_MANIFEST_PATH = "docs/v0.49-release-manifest.json";
const SURFACE_MANIFEST_PATH = "docs/v0.49-agent-surface-manifest.json";
const SOURCE_INVENTORY_PATH = "docs/v0.49-unified-tool-surface-inventory.json";
const SCHEMA_PATH = "crates/aft-subc/src/subc_tool_schemas.json";
const PREVIOUS_SCHEMA_PATH = "crates/aft/src/subc_tool_schemas.json";
const EVIDENCE_PATH = "docs/v0.49-release-evidence.json";
let snapshotDir;

function recordPath(path) {
  return join(snapshotDir, basename(path));
}
// The generation label identifies the publication/activation format introduced
// in v0.49.0, not the current package version. Later releases keep that label;
// disposable snapshots record their own source commit and current source bytes.
// Changing the format and renaming docs/v0.49-* requires a deliberate migration.
const TARGET_VERSION = "0.49.0";
const PREVIOUS_VERSION = "0.48.1";
// The version actually being released. Version-consistency checks compare
// against this; surface-identity checks keep TARGET_VERSION. The gate
// governs every 0.x line from 0.49 onward as a RANGE, not an enumerated
// list: three releases in a row (v0.51.0, v0.52.x, v0.53.0) failed at npm
// publish because a new minor was tagged before its line was registered,
// and the surface-identity checks below are the real protection anyway.
// Revisit the bound when the workspace reaches 1.0 (new surface generation).
const MIN_GOVERNED = { major: 0, minor: 49 };
const RELEASE_VERSION = (() => {
  const cargo = readFileSync(join(ROOT, "crates/aft/Cargo.toml"), "utf8");
  const version = /^version\s*=\s*"([^"]+)"/m.exec(cargo)?.[1];
  if (!version) fail("could not read the workspace version from crates/aft/Cargo.toml");
  const [major, minor] = version.split(".").map((part) => Number.parseInt(part, 10));
  if (
    !Number.isInteger(major) ||
    !Number.isInteger(minor) ||
    major !== MIN_GOVERNED.major ||
    minor < MIN_GOVERNED.minor
  )
    fail(`workspace version ${version} is outside the version range this gate governs`);
  return version;
})();
const BUN = process.env.BUN_BIN || "bun";

function fail(message) {
  throw new Error(message);
}

function readJson(file) {
  try {
    const absolute = isAbsolute(file) ? file : join(ROOT, file);
    return JSON.parse(readFileSync(absolute, "utf8"));
  } catch (error) {
    fail(`could not read ${file}: ${error instanceof Error ? error.message : String(error)}`);
  }
}

function hash(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function exactFile(path) {
  const bytes = readFileSync(join(ROOT, path));
  return { byte_length: bytes.byteLength, sha256: hash(bytes) };
}

function git(args, encoding = "utf8") {
  return execFileSync("git", args, { cwd: ROOT, encoding }).toString().trim();
}

function sourceBytes(commit, path) {
  try {
    // An artifact that did not exist at the previous release commit is a
    // legitimate `present: false` row, so git's "exists on disk, but not in
    // <commit>" complaint is expected output here, not a failure to surface.
    return execFileSync("git", ["show", `${commit}:${path}`], {
      cwd: ROOT,
      encoding: "buffer",
      maxBuffer: 64 * 1024 * 1024,
      stdio: ["ignore", "pipe", "ignore"],
    });
  } catch {
    return null;
  }
}

function assertEqual(actual, expected, label) {
  if (actual !== expected) fail(`${label}: expected ${expected}, got ${actual}`);
}

function parseArgs(argv) {
  const flags = new Set();
  const values = new Map();
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (
      arg === "--candidate" ||
      arg === "--check-inputs" ||
      arg === "--prepare" ||
      arg === "--build" ||
      arg === "--stage" ||
      arg === "--require-platform-artifacts" ||
      arg === "--skip-steering" ||
      arg === "--skip-subc"
    ) {
      flags.add(arg);
      continue;
    }
    if (
      arg === "--snapshot-dir" ||
      arg === "--evidence" ||
      arg === "--evidence-output" ||
      arg === "--platform-assets-dir"
    ) {
      const value = argv[index + 1];
      if (!value || value.startsWith("--")) fail(`${arg} requires a value`);
      values.set(arg, value);
      index += 1;
      continue;
    }
    fail(`unknown argument: ${arg}`);
  }
  return { flags, values };
}

function loadManifests() {
  const release = readJson(recordPath(RELEASE_MANIFEST_PATH));
  const surface = readJson(recordPath(SURFACE_MANIFEST_PATH));
  const inventory = readJson(SOURCE_INVENTORY_PATH);
  const sourceInventory = readJson(surface.source_inventory);
  if (release.artifact_version !== TARGET_VERSION)
    fail("release manifest is not a v0.49.0 manifest");
  if (surface.artifact_version !== TARGET_VERSION)
    fail("agent surface manifest is not a v0.49.0 manifest");
  assertEqual(release.source_commit, surface.source_commit, "release/source manifest commit");
  return { release, surface, inventory, sourceInventory };
}

function checkGovernedBytes(release, surface, sourceInventory) {
  const commit = release.source_commit;
  if (!/^[0-9a-f]{40}$/.test(commit)) fail(`invalid source commit in release manifest: ${commit}`);
  try {
    git(["cat-file", "-e", `${commit}^{commit}`]);
    git(["merge-base", "--is-ancestor", commit, "HEAD"]);
  } catch {
    fail(`manifest source commit is not an ancestor of the candidate: ${commit}`);
  }

  const expectedArtifacts = new Map(surface.artifacts.map((artifact) => [artifact.path, artifact]));
  const governedPaths = sourceInventory.surfaces?.map((surfaceEntry) => surfaceEntry.path) ?? [];
  if (governedPaths.length === 0) fail("S0 source inventory has no governed surfaces");

  const failures = [];
  for (const path of governedPaths) {
    const artifact = expectedArtifacts.get(path);
    if (!artifact) {
      failures.push(`${path}: absent from the agent surface manifest`);
      continue;
    }
    const expected = { byte_length: artifact.byte_length, sha256: artifact.sha256 };
    const current = exactFile(path);
    const committed = sourceBytes(commit, path);
    if (!committed) {
      failures.push(`${path}: absent from manifest source commit ${commit}`);
      continue;
    }
    const committedDigest = { byte_length: committed.byteLength, sha256: hash(committed) };
    if (current.byte_length !== expected.byte_length || current.sha256 !== expected.sha256) {
      failures.push(
        `${path}: working-tree bytes do not match manifest (${current.sha256} != ${expected.sha256})`,
      );
    }
    if (
      committedDigest.byte_length !== expected.byte_length ||
      committedDigest.sha256 !== expected.sha256
    ) {
      failures.push(
        `${path}: source-commit bytes do not match manifest (${committedDigest.sha256} != ${expected.sha256})`,
      );
    }
  }
  if (failures.length > 0) fail(`governed artifact byte gate failed:\n${failures.join("\n")}`);
}

function checkReleasePolicy(release) {
  const policyBytes = sourceBytes(release.source_commit, RELEASE_MANIFEST_PATH);
  if (!policyBytes || hash(policyBytes) !== exactFile(RELEASE_MANIFEST_PATH).sha256) {
    fail("release policy differs from the manifest source commit");
  }
  assertEqual(
    JSON.stringify(release),
    JSON.stringify({ ...readJson(RELEASE_MANIFEST_PATH), source_commit: release.source_commit }),
    "release snapshot policy",
  );
}

function checkDistributionInventory(release, inventory) {
  const declared = inventory.distribution_artifact_manifest;
  if (!declared || declared.manifest_id !== release.distribution_manifest) {
    fail(`release manifest does not point at ${release.distribution_manifest}`);
  }
  const declaredArtifacts = new Map(
    declared.artifacts
      .filter((artifact) => artifact.kind !== "external-prerequisite")
      .map((artifact) => [artifact.id, artifact]),
  );
  const releaseArtifacts = new Map(
    release.independently_versioned_artifacts.map((artifact) => [artifact.id, artifact]),
  );
  for (const [id, artifact] of declaredArtifacts) {
    const candidate = releaseArtifacts.get(id);
    if (!candidate) fail(`release manifest omits S0-declared artifact ${id}`);
    const sourceMatches =
      artifact.source === "later slice artifact" || candidate.source === artifact.source;
    if (
      candidate.name !== artifact.name ||
      !sourceMatches ||
      candidate.version !== artifact.version
    ) {
      fail(`release artifact ${id} does not match S0 declaration`);
    }
  }
  for (const artifact of release.independently_versioned_artifacts) {
    if (artifact.version !== TARGET_VERSION && artifact.id !== "DIST-V049-006") {
      fail(`${artifact.id} is not versioned at ${TARGET_VERSION}`);
    }
  }

  const declaredPlatforms = new Map(
    declared.platform_binary_matrix.map((artifact) => [artifact.id, artifact]),
  );
  const releasePlatforms = new Map(
    release.platform_binary_artifacts.map((artifact) => [artifact.id, artifact]),
  );
  if (declaredPlatforms.size !== releasePlatforms.size) fail("platform binary matrix size changed");
  for (const [id, artifact] of declaredPlatforms) {
    const candidate = releasePlatforms.get(id);
    if (
      !candidate ||
      candidate.name !== artifact.package ||
      candidate.version !== artifact.version
    ) {
      fail(`release manifest does not exactly cover platform artifact ${id}`);
    }
  }
}

function packageJson(path) {
  return readJson(path);
}

function checkCandidateVersions(release, candidate) {
  const packageArtifacts = release.independently_versioned_artifacts.filter(
    (artifact) => artifact.kind === "npm-package",
  );
  const failures = [];
  for (const artifact of packageArtifacts) {
    const packagePath = join(artifact.source, "package.json");
    const pkg = packageJson(packagePath);
    if (candidate && pkg.version !== RELEASE_VERSION)
      failures.push(`${packagePath}: ${pkg.version} != ${RELEASE_VERSION}`);
    if (!candidate && pkg.version !== RELEASE_VERSION && pkg.version !== PREVIOUS_VERSION)
      failures.push(`${packagePath}: unsupported version ${pkg.version}`);
  }

  const cargo = readFileSync(join(ROOT, "crates/aft/Cargo.toml"), "utf8");
  const cargoVersion = /^version\s*=\s*"([^"]+)"/m.exec(cargo)?.[1];
  if (candidate && cargoVersion !== RELEASE_VERSION)
    failures.push(`crates/aft/Cargo.toml: ${cargoVersion} != ${RELEASE_VERSION}`);
  if (!candidate && cargoVersion !== RELEASE_VERSION && cargoVersion !== PREVIOUS_VERSION)
    failures.push(`crates/aft/Cargo.toml: unsupported version ${cargoVersion}`);

  const tokenizer = readFileSync(join(ROOT, "crates/aft-tokenizer/Cargo.toml"), "utf8");
  const tokenizerVersion = /^version\s*=\s*"([^"]+)"/m.exec(tokenizer)?.[1];
  if (candidate && tokenizerVersion !== RELEASE_VERSION)
    failures.push(`crates/aft-tokenizer/Cargo.toml: ${tokenizerVersion} != ${RELEASE_VERSION}`);
  if (!candidate && tokenizerVersion !== RELEASE_VERSION && tokenizerVersion !== PREVIOUS_VERSION)
    failures.push(`crates/aft-tokenizer/Cargo.toml: unsupported version ${tokenizerVersion}`);
  if (failures.length > 0) fail(`candidate version gate failed:\n${failures.join("\n")}`);
}

function run(command, args, cwd = ROOT) {
  console.log(`→ ${command} ${args.join(" ")}`);
  execFileSync(command, args, { cwd, stdio: "inherit", env: process.env });
}

function checkGeneratedSubc(skip, sourceOnly) {
  if (skip) return;
  run(BUN, [
    "packages/opencode-plugin/scripts/build-tool-schemas.ts",
    sourceOnly ? "--check-source-only" : "--check",
  ]);
}

function checkSteering(skip) {
  if (skip) return;
  run(BUN, ["scripts/audit-v049-agent-surface.ts", "--snapshot-dir", snapshotDir]);
}

function prepareSnapshot() {
  mkdirSync(snapshotDir, { recursive: true });
  if (
    [
      RELEASE_MANIFEST_PATH,
      SURFACE_MANIFEST_PATH,
      EVIDENCE_PATH,
      "docs/v0.49-agent-prefix-capture.json",
    ].some((path) => existsSync(recordPath(path)))
  ) {
    fail("snapshot directory already contains publication records; validate it without --prepare");
  }
  // The checked release manifest is policy, not a digest of a moving branch.
  // Only disposable release records acquire a source commit and byte hashes.
  const release = { ...readJson(RELEASE_MANIFEST_PATH), source_commit: git(["rev-parse", "HEAD"]) };
  writeFileSync(recordPath(RELEASE_MANIFEST_PATH), `${JSON.stringify(release, null, 2)}\n`);
  run(BUN, [
    "scripts/audit-v049-agent-surface.ts",
    "--snapshot-dir",
    snapshotDir,
    "--write-prefix-capture",
    "--write-manifest",
  ]);
}

function checkPlatformArtifacts(release, assetsDir) {
  const root = assetsDir ? resolve(ROOT, assetsDir) : ROOT;
  const failures = [];
  for (const artifact of release.platform_binary_artifacts) {
    const pkgPath = join(ROOT, "packages/npm", artifact.directory, "package.json");
    const pkg = packageJson(relative(ROOT, pkgPath));
    if (pkg.version !== RELEASE_VERSION)
      failures.push(`${artifact.id}: ${pkgPath} is ${pkg.version}, not ${RELEASE_VERSION}`);
    const path = assetsDir
      ? join(root, artifact.asset)
      : join(
          root,
          "packages/npm",
          artifact.directory,
          "bin",
          artifact.directory.startsWith("win32") ? "aft.exe" : "aft",
        );
    if (!existsSync(path)) {
      failures.push(`${artifact.id}: missing ${relative(ROOT, path)}`);
      continue;
    }
    const bytes = readFileSync(path);
    if (bytes.byteLength === 0)
      failures.push(`${artifact.id}: empty binary ${relative(ROOT, path)}`);
  }
  if (failures.length > 0) fail(`platform artifact gate failed:\n${failures.join("\n")}`);
}

function buildCandidate(release) {
  // Verify the generated artifact again immediately before native packaging.
  // The preflight check rejects drift; this second check makes the build record
  // prove that the generated-subc artifact stayed unchanged through packaging.
  run(BUN, ["packages/opencode-plugin/scripts/build-tool-schemas.ts", "--check"]);
  for (const artifact of release.independently_versioned_artifacts.filter(
    (entry) => entry.build?.command && entry.kind === "npm-package",
  )) {
    const [command, ...args] = artifact.build.command.split(" ");
    run(command, args, join(ROOT, artifact.build.cwd));
    run("npm", ["pack", "--dry-run", "--json"], join(ROOT, artifact.source));
  }
  // Cargo is intentionally run after all JS builds. This keeps the release
  // gate's cargo work serialized and makes the generated schema check the last
  // source-level check before the native artifact is produced.
  run("cargo", ["build", "--release", "-p", "agent-file-tools"]);
}

function artifactRowsForProfile(surface, sourceInventory, profileId) {
  const paths = sourceInventory.surfaces
    .filter((entry) => entry.profiles.includes(profileId))
    .map((entry) => entry.path);
  const byPath = new Map(surface.artifacts.map((artifact) => [artifact.path, artifact]));
  return paths.map((path) => {
    const artifact = byPath.get(path);
    if (!artifact) fail(`profile ${profileId} has no surface-manifest artifact for ${path}`);
    return { id: artifact.id, path, expected: artifact };
  });
}

function snapshotForCommit(commit, rows) {
  const parts = [];
  for (const row of rows) {
    const bytes = sourceBytes(commit, row.path);
    parts.push(
      JSON.stringify({
        id: row.id,
        path: row.path,
        present: Boolean(bytes),
        byte_length: bytes?.byteLength ?? 0,
        sha256: bytes ? hash(bytes) : null,
      }),
    );
  }
  const value = Buffer.from(`${parts.join("\n")}\n`, "utf8");
  return { complete: true, byte_length: value.byteLength, sha256: hash(value) };
}

function snapshotForManifest(rows) {
  const value = Buffer.from(
    `${rows
      .map((row) =>
        JSON.stringify({
          id: row.id,
          path: row.path,
          present: true,
          byte_length: row.expected.byte_length,
          sha256: row.expected.sha256,
        }),
      )
      .join("\n")}\n`,
    "utf8",
  );
  return { complete: true, byte_length: value.byteLength, sha256: hash(value) };
}

function schemaSnapshot(commit, surface) {
  const oldBytes = sourceBytes(commit, PREVIOUS_SCHEMA_PATH);
  const expected = surface.artifacts.find((artifact) => artifact.path === SCHEMA_PATH);
  if (!expected) fail(`surface manifest omits ${SCHEMA_PATH}`);
  return {
    old: {
      complete: true,
      byte_length: oldBytes?.byteLength ?? 0,
      sha256: oldBytes ? hash(oldBytes) : null,
    },
    candidate: { complete: true, byte_length: expected.byte_length, sha256: expected.sha256 },
  };
}

function validateTransition(profile, expected, transition) {
  if (!Array.isArray(transition.steps) || transition.steps.length !== 2)
    fail(`${profile.id}: transition must contain exactly two observations`);
  const states = transition.steps.map((step) => step.state);
  if (states[0] !== "old" || states[1] !== "candidate")
    fail(`${profile.id}: transition must be old then candidate`);
  if (transition.transition_count !== 1) fail(`${profile.id}: expected exactly one transition`);
  if (transition.mixed_surface !== false) fail(`${profile.id}: mixed surface was observed`);
  for (const [index, state] of ["old", "candidate"].entries()) {
    const step = transition.steps[index];
    const expectedStep = expected[state];
    if (step.agent_prefix.complete !== true || step.generated_subc.complete !== true)
      fail(`${profile.id}/${state}: incomplete capture`);
    assertEqual(
      step.agent_prefix.sha256,
      expectedStep.agent_prefix.sha256,
      `${profile.id}/${state} agent-prefix capture`,
    );
    assertEqual(
      step.generated_subc.sha256,
      expectedStep.generated_subc.sha256,
      `${profile.id}/${state} generated-subc capture`,
    );
  }
  if (transition.cache?.production_key_observable === true) {
    if (transition.cache.invalidation_count !== 1)
      fail(`${profile.id}: observable production cache key must invalidate exactly once`);
  } else if (Object.hasOwn(transition.cache ?? {}, "invalidation_count")) {
    fail(
      `${profile.id}: cache invalidation count is not allowed without an observable production cache key`,
    );
  }
}

function stageTransitions(release, surface, sourceInventory, outputPath) {
  let oldCommit;
  try {
    oldCommit = git(["rev-parse", `v${PREVIOUS_VERSION}^{commit}`]);
  } catch {
    fail(`previous release tag v${PREVIOUS_VERSION} is required for activation staging`);
  }
  const schema = schemaSnapshot(oldCommit, surface);
  const transitions = [];
  for (const profile of release.profiles) {
    const rows = artifactRowsForProfile(surface, sourceInventory, profile.id);
    const oldPrefix = snapshotForCommit(oldCommit, rows);
    const candidatePrefix = snapshotForManifest(rows);
    const expected = {
      old: { agent_prefix: oldPrefix, generated_subc: schema.old },
      candidate: { agent_prefix: candidatePrefix, generated_subc: schema.candidate },
    };
    const stagingRoot = mkdtempSync(join(tmpdir(), `aft-v049-${profile.id.toLowerCase()}-`));
    try {
      const previousPath = join(stagingRoot, "previous.json");
      const candidatePath = join(stagingRoot, "candidate.json");
      writeFileSync(
        previousPath,
        `${JSON.stringify({ version: PREVIOUS_VERSION, artifact: profile.previous_artifact })}\n`,
      );
      writeFileSync(
        candidatePath,
        `${JSON.stringify({ version: TARGET_VERSION, artifact: profile.candidate_artifact })}\n`,
      );
      if (!existsSync(previousPath) || !existsSync(candidatePath))
        fail(`${profile.id}: staging write failed`);
      const transition = {
        profile_id: profile.id,
        harness: profile.harness,
        activation: profile.activation,
        staging: {
          mechanism: profile.activation.mechanism,
          disposable_root: "<disposable-profile-root>",
          atomic_replacement: true,
        },
        steps: [
          {
            state: "old",
            artifact_version: PREVIOUS_VERSION,
            agent_prefix: oldPrefix,
            generated_subc: schema.old,
          },
          {
            state: "candidate",
            artifact_version: TARGET_VERSION,
            agent_prefix: candidatePrefix,
            generated_subc: schema.candidate,
          },
        ],
        transition_count: 1,
        mixed_surface: false,
        cache: { production_key_observable: false, transition_only: true },
      };
      validateTransition(profile, expected, transition);
      transitions.push(transition);
    } finally {
      rmSync(stagingRoot, { recursive: true, force: true });
    }
  }
  const evidence = {
    artifact_id: "ART-V049-S6-ACTIVATION-EVIDENCE-001",
    artifact_version: TARGET_VERSION,
    source_commit: release.source_commit,
    previous_version: PREVIOUS_VERSION,
    candidate_version: TARGET_VERSION,
    capture_rule:
      "Each prefix and generated-subc value is a complete byte snapshot. A valid profile has exactly old -> candidate and no mixed snapshot.",
    transitions,
    atomicity: release.atomicity,
    cache_observability: release.cache_observability,
  };
  if (outputPath)
    writeFileSync(resolve(ROOT, outputPath), `${JSON.stringify(evidence, null, 2)}\n`);
  return evidence;
}

function validateEvidence(path, release, surface, sourceInventory) {
  const evidence = readJson(path);
  if (evidence.source_commit !== release.source_commit)
    fail(`activation evidence source commit differs from release manifest`);
  const byId = new Map(
    evidence.transitions?.map((transition) => [transition.profile_id, transition]) ?? [],
  );
  if (byId.size !== release.profiles.length)
    fail("activation evidence does not cover every registration profile");
  const oldCommit = git(["rev-parse", `v${PREVIOUS_VERSION}^{commit}`]);
  const schema = schemaSnapshot(oldCommit, surface);
  for (const profile of release.profiles) {
    const rows = artifactRowsForProfile(surface, sourceInventory, profile.id);
    validateTransition(
      profile,
      {
        old: { agent_prefix: snapshotForCommit(oldCommit, rows), generated_subc: schema.old },
        candidate: { agent_prefix: snapshotForManifest(rows), generated_subc: schema.candidate },
      },
      byId.get(profile.id),
    );
  }
}

function main() {
  const { flags, values } = parseArgs(process.argv.slice(2));
  if (
    flags.has("--check-inputs") &&
    (values.size > 0 ||
      [...flags].some((flag) => flag !== "--check-inputs" && flag !== "--candidate"))
  ) {
    fail("--check-inputs is a read-only source check, not a build or release snapshot command");
  }
  if (!flags.has("--check-inputs") && !values.has("--snapshot-dir")) {
    fail("use --check-inputs for trains, or --snapshot-dir for release records");
  }
  snapshotDir = flags.has("--check-inputs")
    ? mkdtempSync(join(tmpdir(), "aft-publication-inputs-"))
    : resolve(ROOT, values.get("--snapshot-dir"));
  if (snapshotDir === join(ROOT, "docs") || snapshotDir.startsWith(`${join(ROOT, "docs")}${sep}`)) {
    fail("snapshots must not overwrite checked-in docs");
  }
  const evidencePath =
    values.get("--evidence") ?? values.get("--evidence-output") ?? recordPath(EVIDENCE_PATH);
  if (
    values.has("--evidence-output") &&
    !resolve(ROOT, values.get("--evidence-output")).startsWith(`${snapshotDir}${sep}`)
  ) {
    fail("--evidence-output must stay inside the snapshot directory");
  }
  try {
    if (flags.has("--check-inputs") || flags.has("--prepare")) prepareSnapshot();
    const { release, surface, inventory, sourceInventory } = loadManifests();
    checkGovernedBytes(release, surface, sourceInventory);
    checkReleasePolicy(release);
    checkDistributionInventory(release, inventory);
    checkCandidateVersions(release, flags.has("--candidate"));
    checkGeneratedSubc(flags.has("--skip-subc"), flags.has("--check-inputs"));
    checkSteering(flags.has("--skip-steering"));
    if (flags.has("--build")) buildCandidate(release);
    if (flags.has("--require-platform-artifacts"))
      checkPlatformArtifacts(release, values.get("--platform-assets-dir"));
    if (flags.has("--stage") || flags.has("--prepare") || flags.has("--check-inputs")) {
      const evidence = stageTransitions(
        release,
        surface,
        sourceInventory,
        values.get("--evidence-output") ?? recordPath(EVIDENCE_PATH),
      );
      console.log(
        `✓ staged and validated ${evidence.transitions.length} v0.49 activation transitions`,
      );
    }
    validateEvidence(evidencePath, release, surface, sourceInventory);
    console.log(`✓ v0.49 release gates passed (${release.source_commit})`);
  } finally {
    if (flags.has("--check-inputs")) rmSync(snapshotDir, { recursive: true, force: true });
  }
}

main();
