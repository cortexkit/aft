/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as path from "node:path";
import {
  buildSubcRemoteToolSchemasJson,
  buildSubcToolPresets,
  buildSubcToolPresetsJson,
  buildSubcToolSchemas,
  buildSubcToolSchemasJson,
  CONSUMER_ONLY_MARKER,
  SUBC_BARE_TOOL_NAMES,
} from "../subc-tool-schemas.js";

const REPO_ROOT = path.resolve(import.meta.dir, "..", "..", "..", "..");
const ARTIFACT_PATH = path.join(REPO_ROOT, "crates", "aft", "src", "subc_tool_schemas.json");
const PRESETS_PATH = path.join(REPO_ROOT, "crates", "aft", "src", "subc_tool_presets.json");

const REMOTE_PATH = path.join(REPO_ROOT, "crates", "aft", "src", "subc_tool_remote_schemas.json");
const REMOTE_GUIDANCE =
  'When remote runs are available, put `runon: "linux"` on build and test lines (cargo, bun test), including chains and pipes. Keep git, gh, interactive and file-editing commands local, and keep a line local if it needs macOS (Seatbelt, codesign, launchd, TCC, AppKit) or runs binaries built on this machine: a remote build leaves no binaries or target/ output in the local worktree. Add `,Nc` to request N vCPUs (`linux,4c`); the job sees exactly N CPUs, so ask for what the command uses: 2c for a single test binary or script, 4c for `cargo check`/clippy on one crate, 8c for a workspace test suite, and 16c for a large release build.';

const PLACEHOLDER = JSON.stringify({ type: "object" });

describe("subc tool schemas artifact", () => {
  test("default schemas omit the remote parameter and guidance", () => {
    const schemas = [
      ...Object.values(buildSubcToolSchemas()),
      ...Object.values(buildSubcToolPresets().worker),
      ...Object.values(JSON.parse(fs.readFileSync(ARTIFACT_PATH, "utf8"))),
      ...Object.values(JSON.parse(fs.readFileSync(PRESETS_PATH, "utf8")).worker),
    ] as Array<{ properties?: Record<string, unknown>; description: string }>;
    for (const schema of schemas) {
      expect(schema.properties?.runon).toBeUndefined();
      expect(schema.description).not.toContain("runon");
    }
  });

  test("enabled bash schemas include the remote parameter and guidance", () => {
    const head = buildSubcToolSchemas(true);
    const worker = buildSubcToolPresets(true).worker;
    for (const bash of [head.bash, worker.bash]) {
      expect((bash.properties as Record<string, unknown>).runon).toMatchObject({ type: "string" });
      expect((bash.properties as Record<string, unknown>).runon).toMatchObject({
        description:
          'Run on the remote Linux build server: `linux`, optionally with an exact vCPU count such as `linux,4c`; add ",net" for outbound internet (offline by default).',
      });
      expect((bash.description as string).split(REMOTE_GUIDANCE)).toHaveLength(2);
    }
    for (const schemas of [head, worker]) {
      for (const [name, schema] of Object.entries(schemas)) {
        if (name !== "bash") expect(schema.description as string).not.toContain("runon");
      }
    }
  });

  test("committed enabled-only artifact matches in-memory generation byte-for-byte", () => {
    expect(buildSubcRemoteToolSchemasJson()).toBe(fs.readFileSync(REMOTE_PATH, "utf8"));
  });

  test("committed artifact matches in-memory generation byte-for-byte", () => {
    const committed = fs.readFileSync(ARTIFACT_PATH, "utf8");
    const fresh = buildSubcToolSchemasJson();
    expect(fresh).toBe(committed);
  });

  test("committed preset artifact matches in-memory generation byte-for-byte", () => {
    expect(buildSubcToolPresetsJson()).toBe(fs.readFileSync(PRESETS_PATH, "utf8"));
  });

  test("worker preset texts never promise a wake-up or tell the worker to end its turn", () => {
    const presets = JSON.parse(fs.readFileSync(PRESETS_PATH, "utf8")) as Record<
      string,
      Record<string, Record<string, unknown>>
    >;
    expect(Object.keys(presets.worker).sort()).toEqual([
      "bash",
      "bash_status",
      "bash_watch",
      "powershell",
    ]);
    for (const [tool, schema] of Object.entries(presets.worker)) {
      const text = JSON.stringify(schema).toLowerCase();
      for (const phrase of ["completion reminder", "end the turn", "end your turn", "remind you"]) {
        expect(text.includes(phrase), `${tool}: ${phrase}`).toBe(false);
      }
    }
    expect(presets.worker.bash_watch.description as string).toContain("never wakes you");
  });

  test("all bare names present with object schemas", () => {
    expect(SUBC_BARE_TOOL_NAMES).toHaveLength(24);
    const parsed = JSON.parse(fs.readFileSync(ARTIFACT_PATH, "utf8")) as Record<
      string,
      Record<string, unknown>
    >;
    for (const name of SUBC_BARE_TOOL_NAMES) {
      expect(parsed[name]).toBeDefined();
      expect(parsed[name].type).toBe("object");
      expect(typeof parsed[name].description).toBe("string");
      expect((parsed[name].description as string).length).toBeGreaterThan(0);
    }
    expect(Object.keys(parsed).sort()).toEqual([...SUBC_BARE_TOOL_NAMES].sort());
    // The manifest is shared by trusted and untrusted subc consumers, so it must
    // retain the global gate-off surface and never advertise denied GitHub reads.
    expect(parsed.read.description).not.toContain("issue://NUMBER");
  });

  test("schemas are not bare placeholders (except status empty-object contract)", () => {
    const parsed = JSON.parse(fs.readFileSync(ARTIFACT_PATH, "utf8")) as Record<
      string,
      Record<string, unknown>
    >;
    for (const [name, schema] of Object.entries(parsed)) {
      const serialized = JSON.stringify(schema);
      if (name === "status") {
        expect(schema.properties).toEqual({});
        expect(schema.additionalProperties).toBe(false);
        continue;
      }
      expect(serialized).not.toBe(PLACEHOLDER);
      const props = schema.properties as Record<string, unknown> | undefined;
      expect(props && Object.keys(props).length).toBeGreaterThan(0);
    }
  });

  test("consumer-set properties carry the consumer-only marker the manifest strips", () => {
    // The Rust manifest removes marked properties before serving the catalog;
    // an unmarked consumer flag would reach models and invite them to set it.
    const parsed = JSON.parse(fs.readFileSync(ARTIFACT_PATH, "utf8")) as Record<
      string,
      { properties?: Record<string, Record<string, unknown>> }
    >;
    const marked: string[] = [];
    for (const [tool, schema] of Object.entries(parsed)) {
      for (const [name, property] of Object.entries(schema.properties ?? {})) {
        const description = typeof property.description === "string" ? property.description : "";
        if (description.includes("Consumer-set")) {
          expect(property[CONSUMER_ONLY_MARKER], `${tool}.${name}`).toBe(true);
        }
        if (property[CONSUMER_ONLY_MARKER] === true) marked.push(`${tool}.${name}`);
      }
    }
    for (const tool of ["bash", "powershell"]) {
      for (const flag of ["foreground_orchestrate", "block_to_completion", "shell"]) {
        expect(marked).toContain(`${tool}.${flag}`);
      }
    }
  });
});
