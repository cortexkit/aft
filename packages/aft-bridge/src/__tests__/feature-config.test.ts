/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  legacyConfigNoticeMessage,
  noticeDigest,
  noticeProjection,
  SEMANTIC_COST_NOTICE,
  semanticCostNotice,
  suppliesSemanticIndexInput,
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
) as { introduced_minor: string; reject_from_minor?: string; paths: Record<string, string> };

const roots: string[] = [];

afterEach(() => {
  for (const root of roots.splice(0)) {
    chmodSync(root, 0o755);
    rmSync(root, { recursive: true, force: true });
  }
});

describe("feature-config policy", () => {
  test("experimental aliases preserve opt-in defaults and canonical precedence in both tiers", () => {
    for (const tier of ["user", "project"] as const) {
      for (const [key, expected] of [
        ["experimental_lsp_ty", { experimental: { lsp_ty: true } }],
        [
          "experimental_bash_rewrite",
          { bash: { rewrite: true, compress: false, background: false } },
        ],
        [
          "experimental_bash_compress",
          { bash: { rewrite: false, compress: true, background: false } },
        ],
        [
          "experimental_bash_background",
          { bash: { rewrite: false, compress: false, background: true } },
        ],
      ] as const) {
        const doc: Record<string, unknown> = { [key]: true };
        const out = translateConfigDocument(doc, tier);
        expect(doc).toEqual(expected);
        expect(out.retiredKeys).toContain(key);
        expect(translateConfigDocument(doc, tier).legacyInput).toBe(false);
      }
      const conflict: Record<string, unknown> = {
        experimental_lsp_ty: true,
        experimental_bash_rewrite: true,
        experimental: { lsp_ty: false, bash: { rewrite: false } },
        bash: { compress: true },
      };
      const out = translateConfigDocument(conflict, tier);
      expect(conflict).toEqual({ experimental: { lsp_ty: false }, bash: { compress: true } });
      expect(out.warnings.map((warning) => warning.code)).toEqual([
        "superseded_legacy_config",
        "superseded_legacy_config",
        "superseded_legacy_config",
      ]);
      const nested: Record<string, unknown> = {
        harnesses: {
          pi: {
            experimental: { bash: { background: true, long_running_reminder_enabled: false } },
          },
        },
      };
      translateConfigDocument(nested, tier);
      expect(nested).toEqual({
        harnesses: {
          pi: {
            bash: {
              rewrite: false,
              compress: false,
              background: true,
              long_running_reminder_enabled: false,
            },
          },
        },
      });
      const tuning: Record<string, unknown> = {
        experimental: { bash: { long_running_reminder_enabled: false } },
      };
      expect(translateConfigDocument(tuning, tier).legacyInput).toBe(false);
      expect(tuning).toEqual({ experimental: { bash: { long_running_reminder_enabled: false } } });
    }
  });

  test("notice digests match the shared Rust/TypeScript fixtures", () => {
    for (const fixture of fixtures.cases) {
      expect(noticeDigest(noticeProjection(fixture.doc)), fixture.name).toBe(fixture.digest);
    }
  });

  test("the shared artifact names no rejection version", () => {
    expect(policy.introduced_minor).toBe("0.58");
    expect(policy.reject_from_minor).toBeUndefined();
    expect(Object.keys(policy.paths).some((path) => path.startsWith("gh_"))).toBe(false);
  });

  test("retired keys translate in either tier, are listed, and never error", () => {
    const doc: Record<string, unknown> = {
      disabled_tools: ["aft_glob"],
      search_index: true,
      harnesses: { pi: { semantic_search: false } },
    };
    const out = translateConfigDocument(doc, "user");
    expect(doc).toEqual({
      disabled_tools: ["glob"],
      indexes: { trigram: true },
      harnesses: { pi: { indexes: { semantic: false } } },
    });
    expect(out.legacyInput).toBe(true);
    expect(out.retiredKeys).toEqual(["aft_glob", "harnesses.pi.semantic_search", "search_index"]);
    expect("errors" in out).toBe(false);
  });

  test("GitHub aliases translate with doctor --fix precedence", () => {
    const doc: Record<string, unknown> = {
      gh_read: { enabled: true },
      gh_shim: { enabled: false, binary_path: "/opt/aft" },
    };
    const out = translateConfigDocument(doc, "project");
    expect(doc).toEqual({
      github: { read: true, shim: false },
      gh_shim: { binary_path: "/opt/aft" },
    });
    expect(out.retiredKeys).toEqual(["gh_read", "gh_shim.enabled"]);

    const canonical: Record<string, unknown> = {
      gh_read: { enabled: true },
      github: { read: false },
    };
    const conflict = translateConfigDocument(canonical, "user");
    expect(canonical).toEqual({ github: { read: false } });
    expect(conflict.warnings[0]?.code).toBe("superseded_legacy_config");

    const master: Record<string, unknown> = {
      harnesses: { pi: { gh_shim: { enabled: true }, github: { enabled: false } } },
    };
    translateConfigDocument(master, "user");
    expect(master).toEqual({
      harnesses: { pi: { github: { read: false, write: false, shim: false } } },
    });

    const binaryOnly = translateConfigDocument({ gh_shim: { binary_path: "/opt/aft" } }, "user");
    expect(binaryOnly.legacyInput).toBe(false);
  });

  test("retired inspect and LSP keys translate like doctor --fix", () => {
    const doc: Record<string, unknown> = {
      idle: { lsp_ttl_minutes: 3, root_ttl_minutes: 20 },
      inspect: { tier2_soft_deadline_ms: 50, max_drill_down_items: 20 },
    };
    const out = translateConfigDocument(doc, "user");
    expect(doc).toEqual({ idle: { root_ttl_minutes: 20 }, lsp: { idle_minutes: 5 } });
    expect(out.retiredKeys).toEqual([
      "idle.lsp_ttl_minutes",
      "inspect.max_drill_down_items",
      "inspect.tier2_soft_deadline_ms",
    ]);
    // A whole number written as a float is that number (Rust reads it the
    // same way); a fraction, or an integer beyond the exactly representable
    // range, falls back to the default.
    for (const [text, minutes] of [
      ["12.0", 12],
      ["1e3", 1000],
      ["12.5", 60],
      ["9007199254740993", 60],
    ] as const) {
      const parsed = JSON.parse(`{"idle":{"lsp_ttl_minutes":${text}}}`) as Record<string, unknown>;
      translateConfigDocument(parsed, "user");
      expect(parsed, text).toEqual({ lsp: { idle_minutes: minutes } });
    }
    const nonInteger: Record<string, unknown> = { idle: { lsp_ttl_minutes: "x" } };
    translateConfigDocument(nonInteger, "project");
    expect(nonInteger).toEqual({ lsp: { idle_minutes: 60 } });
    const canonical: Record<string, unknown> = {
      idle: { lsp_ttl_minutes: 10 },
      lsp: { idle_minutes: "never" },
    };
    expect(translateConfigDocument(canonical, "user").warnings[0]?.code).toBe(
      "superseded_legacy_config",
    );
    expect(canonical).toEqual({ lsp: { idle_minutes: "never" } });
  });

  test("a false runtime gate never generates disables and says so", () => {
    for (const gate of [
      { backup: { enabled: false } },
      { inspect: { enabled: false } },
      { bash: false },
      { bash: { enabled: false } },
    ]) {
      for (const tier of ["user", "project"] as const) {
        const doc: Record<string, unknown> = structuredClone(gate);
        const out = translateConfigDocument(doc, tier);
        expect(doc.disabled_tools).toBeUndefined();
        expect(out.legacyInput).toBe(false);
        expect(out.warnings.map((warning) => warning.code)).toContain(
          "legacy_runtime_gate_runtime_only",
        );
      }
    }
    const mixed: Record<string, unknown> = { bash: false, hoist_builtin_tools: false };
    translateConfigDocument(mixed, "user");
    expect(mixed.disabled_tools).toEqual([
      "aft_delete",
      "aft_move",
      "apply_patch",
      "bash",
      "edit",
      "glob",
      "grep",
      "read",
      "write",
    ]);

    const explicit: Record<string, unknown> = { hoist_builtin_tools: false, disabled_tools: [] };
    translateConfigDocument(explicit, "user");
    expect(explicit.disabled_tools).toEqual([]);
    expect(explicit.hoist_builtin_tools).toBeUndefined();
  });

  test("project base blocks contribute only what their own legacy keys imply", () => {
    const hoist: Record<string, unknown> = { hoist_builtin_tools: false };
    translateConfigDocument(hoist, "project");
    expect(hoist.disabled_tools).toEqual([
      "apply_patch",
      "bash",
      "edit",
      "glob",
      "grep",
      "read",
      "write",
    ]);
    const all: Record<string, unknown> = { tool_surface: "all" };
    translateConfigDocument(all, "project");
    expect(all.disabled_tools).toBeUndefined();
  });

  test("only the explicit-list note is delivered once, not per load", () => {
    const fixed: Record<string, unknown> = { hoist_builtin_tools: false, disabled_tools: [] };
    const out = translateConfigDocument(fixed, "user");
    const note = out.warnings.find((warning) => warning.code === "superseded_legacy_config");
    expect(note?.once).toBe(true);
    const conflict = translateConfigDocument(
      { search_index: false, experimental_search_index: true },
      "user",
    );
    expect(conflict.warnings.every((warning) => warning.once !== true)).toBe(true);
  });

  test("the project notice names the file and its retired keys", () => {
    const out = translateConfigDocument(
      { search_index: false, hoist_builtin_tools: true },
      "project",
    );
    expect(legacyConfigNoticeMessage("/repo/.cortexkit/aft.jsonc", out)).toBe(
      "/repo/.cortexkit/aft.jsonc uses retired keys (hoist_builtin_tools, search_index); AFT applied their current equivalents, with the same limits a project config has for those keys. Run `npx @cortexkit/aft doctor --fix` to update the file.",
    );
  });

  test("a retired enabled:false notice says indexes still build and how to stop them", () => {
    const doc: Record<string, unknown> = { enabled: false };
    const out = translateConfigDocument(doc, "user");
    expect(out.retiredEnabledFalse).toBe(true);
    expect(doc.indexes).toBeUndefined();
    expect(out.warnings.map((warning) => warning.code)).toContain(
      "legacy_enabled_false_indexes_still_build",
    );
    const message = legacyConfigNoticeMessage("/cfg/aft.jsonc", out);
    expect(message).toContain("indexes still build");
    expect(message).toContain("indexes.trigram, indexes.semantic and indexes.callgraph to false");

    const other = translateConfigDocument({ tool_surface: "all" }, "user");
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

describe("semantic cost notice", () => {
  test("uses the spec text, with commands a plugin user can run", () => {
    // The spec's wording, but `aft setup` becomes the npx form: the notice
    // reaches users through the plugin, and a plugin user has no `aft`
    // command on PATH.
    expect(SEMANTIC_COST_NOTICE).toBe(
      "AFT indexes now default on; the local semantic backend may download an ONNX runtime and model and use CPU. Run npx @cortexkit/aft setup to change indexes.semantic.",
    );
  });

  test("detects supplied semantic inputs in the base and active harness blocks only", () => {
    expect(suppliesSemanticIndexInput(undefined, "pi")).toBe(false);
    expect(suppliesSemanticIndexInput({ indexes: { trigram: false } }, "pi")).toBe(false);
    expect(suppliesSemanticIndexInput({ indexes: { semantic: false } }, "pi")).toBe(true);
    expect(suppliesSemanticIndexInput({ semantic_search: true }, "pi")).toBe(true);
    expect(suppliesSemanticIndexInput({ experimental_semantic_search: true }, "pi")).toBe(true);
    const harnessOnly = { harnesses: { opencode: { indexes: { semantic: true } } } };
    expect(suppliesSemanticIndexInput(harnessOnly, "opencode")).toBe(true);
    expect(suppliesSemanticIndexInput(harnessOnly, "pi")).toBe(false);
  });

  test("applies only to an effectively-on default local backend and is delivered once", () => {
    const base = {
      userConfigPath: "/cfg/aft.jsonc",
      configFileLoaded: true,
      semanticEffective: true,
      semanticInputSupplied: false,
      semanticBackend: undefined,
    };
    expect(semanticCostNotice({ ...base, semanticEffective: false })).toBeNull();
    expect(semanticCostNotice({ ...base, semanticInputSupplied: true })).toBeNull();
    expect(semanticCostNotice({ ...base, semanticBackend: "ollama" })).toBeNull();
    // A fresh install has no config file that could have relied on the old
    // default, so there is nothing to migrate and nothing to announce.
    expect(semanticCostNotice({ ...base, configFileLoaded: false })).toBeNull();
    const notice = semanticCostNotice({ ...base, semanticBackend: "fastembed" });
    expect(notice?.message).toBe(SEMANTIC_COST_NOTICE);

    const root = mkdtempSync(join(tmpdir(), "aft-cost-notice-"));
    roots.push(root);
    const storePath = join(root, "state", "migration-notices.json");
    const delivered: string[] = [];
    const deliver = (message: string) => delivered.push(message);
    const fresh = semanticCostNotice(base);
    if (fresh === null) throw new Error("expected a cost notice");
    expect(deliverMigrationNoticeOnce({ ...fresh, deliver, storePath })).toBe(true);
    // A later load (a restart) yields the same identity and is suppressed.
    const again = semanticCostNotice(base);
    if (again === null) throw new Error("expected a cost notice");
    expect(deliverMigrationNoticeOnce({ ...again, deliver, storePath })).toBe(false);
    expect(delivered).toEqual([SEMANTIC_COST_NOTICE]);
  });
});
