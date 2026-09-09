import { access, mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";

import { fail } from "./errors.js";
import { asRecord, readJson, runCommand, sha256File } from "./util.js";

export interface ExecutablePolicy {
  schema_version: 1;
  sidecar_name: string;
  allowed_sources: Array<"checkout-build" | "same-sha-artifact">;
  allowed_profiles: string[];
  required_sidecar_fields: string[];
}

export interface ProducerBuildInfo {
  git_sha: string;
  build_profile: string;
  sha256: string;
  source: "checkout-build" | "same-sha-artifact";
}

export interface ExecutableProvenanceManifest extends ProducerBuildInfo {
  executable_path: string;
  observed_sha256: string;
  checkout_git_sha: string;
  version_output: string;
  launch_exit_code: number | null;
  verdict: "verified";
  verified_at: string;
}

function parsePolicy(value: unknown): ExecutablePolicy {
  const record = asRecord(value);
  if (
    record?.schema_version !== 1 ||
    typeof record.sidecar_name !== "string" ||
    !Array.isArray(record.allowed_sources) ||
    !Array.isArray(record.allowed_profiles) ||
    !Array.isArray(record.required_sidecar_fields)
  ) {
    fail("executable_provenance", "committed executable policy is invalid", {}, true);
  }
  return record as unknown as ExecutablePolicy;
}

function parseBuildInfo(value: unknown, policy: ExecutablePolicy): ProducerBuildInfo {
  const record = asRecord(value);
  if (!record) fail("executable_provenance", "producer build-info.json is not an object", {}, true);
  for (const field of policy.required_sidecar_fields) {
    if (!(field in record)) {
      fail("executable_provenance", `producer sidecar missing ${field}`, { field }, true);
    }
  }
  if (typeof record.source !== "string") {
    fail("executable_provenance", "producer sidecar source is missing", {}, true);
  }
  if (!policy.allowed_sources.includes(record.source as ProducerBuildInfo["source"])) {
    fail(
      "executable_provenance",
      `producer sidecar source is disallowed: ${record.source}`,
      { source: record.source },
      true,
    );
  }
  if (
    typeof record.build_profile !== "string" ||
    !policy.allowed_profiles.includes(record.build_profile)
  ) {
    fail(
      "executable_provenance",
      `producer build profile is disallowed: ${String(record.build_profile)}`,
      { build_profile: record.build_profile },
      true,
    );
  }
  if (typeof record.git_sha !== "string" || !/^[0-9a-f]{40}$/i.test(record.git_sha)) {
    fail("executable_provenance", "producer git_sha must be a full commit SHA", {}, true);
  }
  if (typeof record.sha256 !== "string" || !/^[0-9a-f]{64}$/i.test(record.sha256)) {
    fail("executable_provenance", "producer sha256 must be a SHA-256 digest", {}, true);
  }
  return {
    git_sha: record.git_sha,
    build_profile: record.build_profile,
    sha256: record.sha256,
    source: record.source as ProducerBuildInfo["source"],
  };
}

async function checkoutSha(repoRoot: string, supplied?: string): Promise<string> {
  const observed = await runCommand("git", ["rev-parse", "HEAD"], {
    cwd: repoRoot,
    timeoutMs: 5_000,
  }).catch(() => undefined);
  const fromGit = observed?.exit_code === 0 ? observed.stdout.trim() : undefined;
  const sha = fromGit || supplied;
  if (!sha || !/^[0-9a-f]{40}$/i.test(sha)) {
    fail("executable_provenance", "checkout git SHA is unavailable", {}, true);
  }
  if (fromGit && supplied && fromGit !== supplied) {
    fail(
      "executable_provenance",
      "supplied checkout SHA disagrees with git rev-parse HEAD",
      { git_sha: fromGit, supplied_sha: supplied },
      true,
    );
  }
  return sha;
}

export async function verifyExecutableProvenance(options: {
  executable: string;
  repoRoot: string;
  policyPath: string;
  manifestPath: string;
  checkoutSha?: string;
}): Promise<ExecutableProvenanceManifest> {
  const executable = resolve(options.executable);
  await access(executable).catch(() => {
    fail("executable_provenance", `configured executable is missing: ${executable}`, {}, true);
  });
  const policy = parsePolicy(await readJson(options.policyPath));
  const sidecarPath = join(dirname(executable), policy.sidecar_name);
  const buildInfo = parseBuildInfo(
    await readJson(sidecarPath).catch(() => {
      fail("executable_provenance", `producer sidecar is missing: ${sidecarPath}`, {}, true);
    }),
    policy,
  );
  const observedSha = await sha256File(executable);
  if (observedSha !== buildInfo.sha256) {
    fail(
      "executable_provenance",
      "configured executable hash disagrees with producer sidecar",
      { sidecar_sha256: buildInfo.sha256, observed_sha256: observedSha },
      true,
    );
  }
  const head = await checkoutSha(options.repoRoot, options.checkoutSha);
  if (head !== buildInfo.git_sha) {
    fail(
      "executable_provenance",
      "producer git_sha disagrees with checkout HEAD",
      { producer_git_sha: buildInfo.git_sha, checkout_git_sha: head },
      true,
    );
  }
  const launch = await runCommand(executable, ["--version"], {
    cwd: options.repoRoot,
    timeoutMs: 10_000,
  });
  const versionOutput = `${launch.stdout}\n${launch.stderr}`.trim();
  if (launch.exit_code !== 0 || launch.timed_out) {
    fail(
      "executable_provenance",
      "configured executable failed its version launch",
      { exit_code: launch.exit_code, timed_out: launch.timed_out, version_output: versionOutput },
      true,
    );
  }
  if (
    !versionOutput.includes(buildInfo.git_sha) &&
    !versionOutput.includes(buildInfo.git_sha.slice(0, 12))
  ) {
    fail(
      "executable_provenance",
      "configured executable did not self-report the producer git SHA",
      { expected_git_sha: buildInfo.git_sha, version_output: versionOutput },
      true,
    );
  }
  const manifest: ExecutableProvenanceManifest = {
    ...buildInfo,
    executable_path: executable,
    observed_sha256: observedSha,
    checkout_git_sha: head,
    version_output: versionOutput,
    launch_exit_code: launch.exit_code,
    verdict: "verified",
    verified_at: new Date().toISOString(),
  };
  await mkdir(dirname(options.manifestPath), { recursive: true });
  await writeFile(options.manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);

  // Re-read the output so the run credits only durable evidence, not an in-memory verdict.
  const durable = JSON.parse(await readFile(options.manifestPath, "utf8")) as unknown;
  if (asRecord(durable)?.source !== buildInfo.source) {
    fail("executable_provenance", "generated manifest did not preserve producer source", {}, true);
  }
  return manifest;
}
