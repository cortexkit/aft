/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  legacyConfigNoticeMessage,
  noticeDigest,
  noticeProjection,
  policyPhaseForVersion,
  translateConfigDocument,
  validateResolvedConfig,
} from "../feature-config.js";
import { deliverMigrationNoticeOnce } from "../migration-notices.js";

const fixtures = JSON.parse(
  readFileSync(
    new URL(
      "../../../../crates/aft/tests/fixtures/feature_config/notice_projection.json",
      import.meta.url,
    ),
    "utf8",
  ),
) as { cases: Array<{ name: string; doc?: Record<string, unknown>; digest: string }> };

const policy = JSON.parse(
  readFileSync(
    new URL("../../../../spec/feature-config/migration-policy.json", import.meta.url),
    "utf8",
  ),
) as { introduced_minor: string; reject_from_minor: string; paths: Record<string, string> };

const roots: string[] = [];
afterEach(() => {
  for (const root of roots.splice(0)) {
    chmodSync(root, 0o755);
    rmSync(root, { recursive: true, force: true });
  }
});

describe("feature-config policy", () => {
  test("notice digests match the shared Rust/TypeScript fixtures", () => {
    for (const fixture of fixtures.cases) {
      expect(noticeDigest(noticeProjection(fixture.doc)), fixture.name).toBe(fixture.digest);
    }
  });

  test("policy versions come from the shared artifact and compare major/minor only", () => {
    expect(policy.introduced_minor).toBe("0.58");
    expect(policy.reject_from_minor).toBe("0.59");
    expect(Object.keys(policy.paths).some((path) => path.startsWith("gh_"))).toBe(false);
    expect(policyPhaseForVersion("0.58.9")).toBe("window");
    expect(policyPhaseForVersion("0.59.0")).toBe("rejecting");
    expect(policyPhaseForVersion("0.59.3-beta.1")).toBe("rejecting");
  });

  test("gate-only bases union with the default and explicit [] wins", () => {
    const backup: Record<string, unknown> = { backup: { enabled: false } };
    const out = translateConfigDocument(backup, "window", "user");
    expect(backup.disabled_tools).toEqual(["aft_delete", "aft_move", "aft_safety"]);
    expect(out.warnings.map((warning) => warning.code)).toContain(
      "legacy_runtime_gate_requires_fix",
    );

    const explicit: Record<string, unknown> = { hoist_builtin_tools: false, disabled_tools: [] };
    translateConfigDocument(explicit, "window", "user");
    expect(explicit.disabled_tools).toEqual([]);
    expect(explicit.hoist_builtin_tools).toBeUndefined();
  });

  test("project base blocks contribute only what their own legacy keys imply", () => {
    const hoist: Record<string, unknown> = { hoist_builtin_tools: false };
    translateConfigDocument(hoist, "window", "project");
    expect(hoist.disabled_tools).toEqual([
      "apply_patch",
      "bash",
      "edit",
      "glob",
      "grep",
      "read",
      "write",
    ]);
    const backup: Record<string, unknown> = { backup: { enabled: false } };
    translateConfigDocument(backup, "window", "project");
    expect(backup.disabled_tools).toEqual(["aft_safety"]);
    const all: Record<string, unknown> = { tool_surface: "all" };
    translateConfigDocument(all, "window", "project");
    expect(all.disabled_tools).toBeUndefined();
  });

  test("only the retained-gate note on an explicit list is delivered once, not per load", () => {
    const fixed: Record<string, unknown> = { backup: { enabled: false }, disabled_tools: [] };
    const out = translateConfigDocument(fixed, "window", "user");
    const note = out.warnings.find((warning) => warning.code === "superseded_legacy_config");
    expect(note?.once).toBe(true);
    const conflict = translateConfigDocument(
      { search_index: false, experimental_search_index: true },
      "window",
      "user",
    );
    expect(conflict.warnings.every((warning) => warning.once !== true)).toBe(true);
  });

  test("a retired enabled:false notice says indexes still build and how to stop them", () => {
    const doc: Record<string, unknown> = { enabled: false };
    const out = translateConfigDocument(doc, "window", "user");
    expect(out.retiredEnabledFalse).toBe(true);
    expect(doc.indexes).toBeUndefined();
    expect(out.warnings.map((warning) => warning.code)).toContain(
      "legacy_enabled_false_indexes_still_build",
    );
    const message = legacyConfigNoticeMessage("/cfg/aft.jsonc", out);
    expect(message).toContain("indexes still build");
    expect(message).toContain("indexes.trigram, indexes.semantic and indexes.callgraph to false");

    const other = translateConfigDocument({ tool_surface: "all" }, "window", "user");
    expect(other.retiredEnabledFalse).toBe(false);
    expect(legacyConfigNoticeMessage("/cfg/aft.jsonc", other)).not.toContain("indexes still build");
  });

  test("resolved validation reports sorted paths and containers only", () => {
    expect(validateResolvedConfig({})).toEqual([
      "invalid_resolved_config:missing:disabled_tools",
      "invalid_resolved_config:missing:indexes",
    ]);
    expect(
      validateResolvedConfig({ disabled_tools: "x", indexes: { trigram: 1, semantic: true } }),
    ).toEqual([
      "invalid_resolved_config:type:disabled_tools",
      "invalid_resolved_config:missing:indexes.callgraph",
      "invalid_resolved_config:type:indexes.trigram",
    ]);
  });
});

describe("migration notice delivery", () => {
  test("an unchanged identity is delivered once; a changed identity again", () => {
    const root = mkdtempSync(join(tmpdir(), "aft-notice-"));
    roots.push(root);
    const storePath = join(root, "state", "migration-notices.json");
    const delivered: string[] = [];
    const deliver = (message: string) => delivered.push(message);
    const notice = { configPath: join(root, "aft.jsonc"), message: "m", deliver, storePath };

    expect(deliverMigrationNoticeOnce({ ...notice, digest: "a" })).toBe(true);
    expect(deliverMigrationNoticeOnce({ ...notice, digest: "a" })).toBe(false);
    expect(deliverMigrationNoticeOnce({ ...notice, digest: "b" })).toBe(true);
    expect(delivered).toEqual(["m", "m"]);
  });

  test("an unwritable state directory still delivers and says suppression cannot persist", () => {
    const root = mkdtempSync(join(tmpdir(), "aft-notice-ro-"));
    roots.push(root);
    chmodSync(root, 0o555);
    const delivered: string[] = [];
    deliverMigrationNoticeOnce({
      configPath: join(root, "aft.jsonc"),
      digest: "a",
      message: "m",
      deliver: (message) => delivered.push(message),
      storePath: join(root, "state", "migration-notices.json"),
    });
    expect(delivered).toHaveLength(1);
    expect(delivered[0]).toContain("may repeat");
  });
});
