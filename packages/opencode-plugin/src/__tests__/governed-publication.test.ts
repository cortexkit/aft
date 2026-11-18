import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, dirname, join, resolve } from "node:path";

const root = resolve(import.meta.dir, "../../../..");
const gate = "scripts/release-gate-v049.mjs";
const manifest = "v0.49-agent-surface-manifest.json";
const maintenanceTrace = ".git/fixture-maintenance.jsonl";
const surfaceA = "packages/opencode-plugin/src/tools/reading.ts";
const surfaceB = "packages/pi-plugin/src/tools/reading.ts";
let fixture: string;

function command(cwd: string, program: string, args: string[], env: NodeJS.ProcessEnv = {}) {
  const result = spawnSync(
    program,
    program === "git" ? ["-c", `safe.directory=${cwd}`, ...args] : args,
    {
      cwd,
      encoding: "utf8",
      env: {
        ...process.env,
        ...env,
        BUN_BIN: process.execPath,
        GIT_CONFIG_GLOBAL: "/dev/null",
        GIT_CONFIG_NOSYSTEM: "1",
        PATH: `${join(fixture ?? root, "capture-bin")}${delimiter}${process.env.PATH}`,
      },
    },
  );
  return { status: result.status, output: `${result.stdout ?? ""}${result.stderr ?? ""}` };
}

function git(...args: string[]) {
  const result = command(fixture, "git", args);
  expect(result.status, result.output).toBe(0);
  return result.output.trim();
}

function check(...args: string[]) {
  // These release-record tests exercise real byte/evidence validators, not
  // native schema compilation. CI and release run the unskipped native gate.
  const releaseFlags = args.includes("--snapshot-dir") ? ["--skip-subc"] : [];
  return command(fixture, "node", [gate, ...args, ...releaseFlags]);
}

function green(...args: string[]) {
  const result = check(...args);
  expect(result.status, result.output).toBe(0);
  expect(result.output).toContain("release gates passed");
  expect(result.output).not.toContain("cargo build");
}

beforeAll(() => {
  fixture = mkdtempSync(join(tmpdir(), "aft-governed-publication-"));
  mkdirSync(join(fixture, "capture-bin"));
  for (const host of ["opencode", "pi"]) {
    writeFileSync(join(fixture, "capture-bin", host), "#!/bin/sh\nprintf 'test host\\n'\n", {
      mode: 0o755,
    });
  }
  writeFileSync(
    join(fixture, "capture-bin", "cargo"),
    "#!/bin/sh\necho 'Unexpected native build in source-only gate' >&2\nexit 97\n",
    { mode: 0o755 },
  );
  git("init", "-q");
  writeFileSync(
    join(fixture, ".git/info/exclude"),
    "capture-bin/\nnode_modules\npackages/aft-bridge/dist\n",
  );
  git("config", "user.name", "Publication test");
  git("config", "user.email", "publication-test@example.invalid");
  git("config", "commit.gpgsign", "false");
  // Rapid fixture commits can launch overlapping detached repacks, making
  // objects unreadable while the publication gate checks their ancestry.
  git("config", "maintenance.auto", "false");
  // Use a sparse, distinct previous-release fixture so the tests work in a
  // shallow checkout. Production gates still require the real release tag.
  for (const [path, bytes] of [
    ["docs/tools.md", "Previous release fixture tools.\n"],
    ["crates/aft/src/subc_tool_schemas.json", '{"tools": []}\n'],
  ]) {
    mkdirSync(dirname(join(fixture, path)), { recursive: true });
    writeFileSync(join(fixture, path), bytes);
  }
  git("add", ".");
  const initial = command(
    fixture,
    "git",
    ["commit", "--allow-empty", "-qm", "previous-release surface bytes"],
    { GIT_TRACE2_EVENT: join(fixture, maintenanceTrace) },
  );
  expect(initial.status, initial.output).toBe(0);
  git("tag", "v0.48.1");
  const tracked = command(root, "git", ["ls-files", "-z"]);
  expect(tracked.status, tracked.output).toBe(0);
  for (const path of tracked.output.split("\0").filter(Boolean)) {
    const source = join(root, path);
    if (!existsSync(source)) continue;
    mkdirSync(dirname(join(fixture, path)), { recursive: true });
    copyFileSync(source, join(fixture, path));
  }
  git("add", ".");
  git("commit", "-qm", "shared origin/main inputs");
  git("branch", "-M", "main");
  git("update-ref", "refs/remotes/origin/main", "HEAD");
  for (const path of [
    "node_modules",
    "packages/opencode-plugin/node_modules",
    "packages/pi-plugin/node_modules",
    "packages/aft-bridge/node_modules",
    "packages/aft-bridge/dist",
  ]) {
    rmSync(join(fixture, path), { recursive: true, force: true });
    symlinkSync(
      join(root, path),
      join(fixture, path),
      process.platform === "win32" ? "junction" : "dir",
    );
  }
}, 120_000);

afterAll(() => {
  if (fixture) rmSync(fixture, { recursive: true, force: true });
});

describe("governed publication", () => {
  test("fixture commits do not start automatic Git maintenance", () => {
    const trace = readFileSync(join(fixture, maintenanceTrace), "utf8").trim();
    const events = trace.split("\n").map((line) => JSON.parse(line));
    expect(events).toContainEqual(expect.objectContaining({ event: "cmd_name", name: "commit" }));
    const maintenance = events.filter(
      (entry) =>
        entry.event === "child_start" &&
        entry.argv?.includes("--auto") &&
        (entry.argv.includes("maintenance") || entry.argv.includes("gc")),
    );
    expect(maintenance).toEqual([]);
  });

  test("independent trains merge without generated commits or regeneration", () => {
    for (const [branch, path] of [
      ["train/first", surfaceA],
      ["train/second", surfaceB],
    ]) {
      git("checkout", "-qb", branch, "origin/main");
      writeFileSync(
        join(fixture, path),
        `${readFileSync(join(fixture, path), "utf8")}\n// Independent source change.\n`,
      );
      git("add", path);
      git("commit", "-qm", branch);
      const before = git("rev-parse", "HEAD");
      green("--check-inputs");
      expect(git("rev-parse", "HEAD")).toBe(before);
      expect(git("status", "--porcelain")).toBe("");
      expect(git("diff", "--name-only", "origin/main...HEAD")).toBe(path);
    }
    git("checkout", "-q", "main");
    git("merge", "--ff-only", "train/first");
    git("checkout", "-q", "train/second");
    git("merge", "--no-edit", "main");
    const merged = git("rev-parse", "HEAD");
    green("--check-inputs");
    const alias = command(fixture, "bash", ["scripts/align-governed-docs.sh"]);
    expect(alias.status, alias.output).toBe(0);
    expect(git("rev-parse", "HEAD")).toBe(merged);
    expect(git("status", "--porcelain")).toBe("");
  }, 300_000);

  test("release snapshots reject stale or tampered governed bytes", () => {
    const snapshot = mkdtempSync(join(tmpdir(), "aft-release-bytes-"));
    const source = join(fixture, surfaceA);
    const bytes = readFileSync(source, "utf8");
    try {
      green("--prepare", "--snapshot-dir", snapshot, "--candidate");
      green("--snapshot-dir", snapshot, "--candidate");
      const path = join(snapshot, manifest);
      const original = readFileSync(path, "utf8");
      const changed = Buffer.from(`${bytes}\n// Changed after release snapshot.\n`);
      writeFileSync(source, changed);
      // Even matching working bytes, forged digests and restaged activation
      // evidence cannot replace the committed candidate bytes.
      const forged = JSON.parse(original);
      const artifact = forged.artifacts.find((entry: { path: string }) => entry.path === surfaceA);
      artifact.byte_length = changed.byteLength;
      artifact.sha256 = createHash("sha256").update(changed).digest("hex");
      writeFileSync(path, `${JSON.stringify(forged, null, 2)}\n`);
      const committed = check("--snapshot-dir", snapshot, "--candidate", "--stage");
      expect(committed.status, committed.output).not.toBe(0);
      expect(committed.output).toContain("source-commit bytes do not match manifest");
      writeFileSync(path, original);
      const stale = check("--snapshot-dir", snapshot, "--candidate");
      expect(stale.status, stale.output).not.toBe(0);
      expect(stale.output).toContain("governed artifact byte gate failed");
      writeFileSync(source, bytes);
      const tampered = JSON.parse(original);
      tampered.artifacts.find((entry: { path: string }) => entry.path === surfaceA).sha256 =
        "0".repeat(64);
      writeFileSync(path, `${JSON.stringify(tampered, null, 2)}\n`);
      const digest = check("--snapshot-dir", snapshot, "--candidate");
      expect(digest.status, digest.output).not.toBe(0);
      expect(digest.output).toContain("governed artifact byte gate failed");
      writeFileSync(path, original);
      green("--snapshot-dir", snapshot, "--candidate");
    } finally {
      writeFileSync(source, bytes);
      rmSync(snapshot, { recursive: true, force: true });
    }
  }, 300_000);

  test("release snapshots reject changed evidence, prefix captures and policy", () => {
    const snapshot = mkdtempSync(join(tmpdir(), "aft-release-records-"));
    try {
      green("--prepare", "--snapshot-dir", snapshot, "--candidate");
      for (const [name, message, change] of [
        [
          "v0.49-release-evidence.json",
          "mixed surface was observed",
          (record: { transitions: Array<{ mixed_surface: boolean }> }) => {
            record.transitions[0].mixed_surface = true;
          },
        ],
        [
          "v0.49-agent-prefix-capture.json",
          "exact bytes changed",
          (record: { capture_scope: string }) => {
            record.capture_scope = "tampered";
          },
        ],
        [
          "v0.49-release-manifest.json",
          "release snapshot policy",
          (record: { profiles: unknown[] }) => {
            record.profiles = [];
          },
        ],
      ] as const) {
        const path = join(snapshot, name);
        const original = readFileSync(path, "utf8");
        const record = JSON.parse(original);
        change(record);
        writeFileSync(path, `${JSON.stringify(record, null, 2)}\n`);
        const result = check("--snapshot-dir", snapshot, "--candidate");
        expect(result.status, result.output).not.toBe(0);
        expect(result.output).toContain(message);
        writeFileSync(path, original);
      }
      const repeat = check("--prepare", "--snapshot-dir", snapshot, "--candidate");
      expect(repeat.status, repeat.output).not.toBe(0);
      expect(repeat.output).toContain("already contains publication records");
      green("--snapshot-dir", snapshot, "--candidate");
      const nativeBuild = check("--check-inputs", "--build");
      expect(nativeBuild.status, nativeBuild.output).not.toBe(0);
      expect(nativeBuild.output).toContain("read-only source check");
    } finally {
      rmSync(snapshot, { recursive: true, force: true });
    }
  }, 300_000);

  test("source checks never author a missing reviewed allowlist", () => {
    const path = join(fixture, "docs/v0.49-legacy-vocabulary-allowlist.json");
    const original = readFileSync(path);
    try {
      rmSync(path);
      const missing = check("--check-inputs");
      expect(missing.status, missing.output).not.toBe(0);
      expect(missing.output).toContain("reviewed legacy-vocabulary allowlist is required");
      expect(existsSync(path)).toBe(false);
    } finally {
      writeFileSync(path, original);
    }
  }, 30_000);
});
