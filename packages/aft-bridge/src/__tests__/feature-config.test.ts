/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  DATA_LAYER_KEYS_REFUSED_FROM,
  DATA_LAYER_RETIRED_PATHS,
  dropRemovedDataLayerKeys,
  getRunningRelease,
  LEGACY_TOOL_ALIASES,
  legacyConfigNoticeMessage,
  noticeDigest,
  noticeProjection,
  parseRelease,
  REFUSED_RETIRED_PATHS,
  REMOVED_DATA_LAYER_NOTES,
  RETIRED_ENABLED_FALSE_INDEXES_NOTE,
  RETIRED_HANG_THRESHOLD_NOTE,
  removedConfigKeyErrors,
  retiredIdleCheckoutHours,
  retiredIdleCheckoutHoursNote,
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

const removedKeyFixtures = JSON.parse(
  readFileSync(
    new URL(
      "../../../../crates/aft/tests/fixtures/feature_config/removed_config_key_errors.json",
      import.meta.url,
    ),
    "utf8",
  ),
) as {
  cases: Array<{ name: string; release?: string; doc: Record<string, unknown>; errors: string[] }>;
};

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
  test("removed config key checking matches shared Rust/TypeScript fixtures", () => {
    for (const fixture of removedKeyFixtures.cases) {
      // A case may name the release that judges it; the data-layer keys are
      // refused only from DATA_LAYER_KEYS_REFUSED_FROM.
      const release = fixture.release === undefined ? undefined : parseRelease(fixture.release);
      expect(removedConfigKeyErrors(fixture.doc, release), fixture.name).toEqual(fixture.errors);
    }
  });

  test("every refused retired name is removed by the translation doctor --fix uses", () => {
    const documents: Array<[string, Record<string, unknown>]> = [
      ["sandbox.enabled", { sandbox: { enabled: true } }],
      ["tool_surface", { tool_surface: "minimal" }],
      ["hoist_builtin_tools", { hoist_builtin_tools: false }],
      ["enabled", { enabled: false }],
      ["search_index", { search_index: false }],
      ["experimental_search_index", { experimental_search_index: false }],
      ["semantic_search", { semantic_search: false }],
      ["experimental_semantic_search", { experimental_semantic_search: false }],
      ["callgraph_store", { callgraph_store: false }],
      ["github.enabled", { github: { enabled: false } }],
      ["gh_read", { gh_read: { enabled: true } }],
      ["gh_shim.enabled", { gh_shim: { enabled: false, binary_path: "/opt/aft/bin/aft" } }],
      ["experimental_lsp_ty", { experimental_lsp_ty: true }],
      ["experimental_bash_rewrite", { experimental_bash_rewrite: true }],
      ["experimental_bash_compress", { experimental_bash_compress: true }],
      ["experimental_bash_background", { experimental_bash_background: true }],
      ["experimental.bash", { experimental: { bash: { background: true } } }],
      [
        "experimental.bash",
        { experimental: { bash: { long_running_reminder_interval_ms: 5000 } } },
      ],
      ["idle.lsp_ttl_minutes", { idle: { lsp_ttl_minutes: 10 } }],
      ["inspect.tier2_soft_deadline_ms", { inspect: { tier2_soft_deadline_ms: 50 } }],
      ["inspect.max_drill_down_items", { inspect: { max_drill_down_items: 20 } }],
      ["views.enabled", { views: { enabled: true } }],
      ["views.callgraph_copies", { views: { callgraph_copies: 2 } }],
      ["worktree.ram_overlay", { worktree: { ram_overlay: false } }],
      ...Object.keys(LEGACY_TOOL_ALIASES).map((name): [string, Record<string, unknown>] => [
        name,
        { disabled_tools: [name] },
      ]),
    ];
    const listed = new Set(documents.map(([name]) => name));
    for (const path of REFUSED_RETIRED_PATHS) expect(listed.has(path), path).toBe(true);
    for (const [name, base] of documents) {
      for (const [prefix, doc] of [
        ["", structuredClone(base)],
        ["harnesses.pi.", { harnesses: { pi: structuredClone(base) } }],
      ] as const) {
        const label = JSON.stringify(doc);
        expect(removedConfigKeyErrors(doc), label).toEqual([
          `removed_config_key:${prefix}${name}; run npx @cortexkit/aft doctor --fix`,
        ]);
        // False keeps project files loadable; plugin user checks are stricter.
        expect(removedConfigKeyErrors(doc, getRunningRelease(), false), label).toEqual([]);
        for (const tier of ["user", "project"] as const) {
          const translated = structuredClone(doc);
          const out = translateConfigDocument(translated, tier);
          expect(out.legacyInput, label).toBe(true);
          expect(removedConfigKeyErrors(translated), `${tier}: ${label}`).toEqual([]);
        }
      }
    }
  });

  test("sandbox profile cutover maps enabled by role and canonical leaves win", () => {
    for (const enabled of [true, false]) {
      const document: Record<string, unknown> = { sandbox: { enabled } };
      const translated = translateConfigDocument(document, "user");
      expect(document.sandbox).toEqual({
        profile: enabled
          ? { worker: "strict", head: "strict" }
          : { worker: "relaxed", head: "off" },
      });
      expect(translated.retiredKeys).toContain("sandbox.enabled");
    }
    const document: Record<string, unknown> = {
      sandbox: { enabled: true, profile: { head: "off" } },
      harnesses: { pi: { sandbox: { enabled: false, profile: { worker: "strict" } } } },
    };
    translateConfigDocument(document, "user");
    expect(document.sandbox).toEqual({ profile: { worker: "strict", head: "off" } });
    expect((document.harnesses as Record<string, unknown>).pi).toEqual({
      sandbox: { profile: { worker: "strict", head: "off" } },
    });
  });
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
      // A block with only reminder settings is retired too: its settings
      // move to top-level `bash` without setting the feature flags, so they
      // keep their current defaults.
      const tuning: Record<string, unknown> = {
        experimental: { bash: { long_running_reminder_enabled: false } },
      };
      const tuningOut = translateConfigDocument(tuning, tier);
      expect(tuningOut.legacyInput).toBe(true);
      expect(tuningOut.retiredKeys).toEqual(["experimental.bash.long_running_reminder_enabled"]);
      expect(tuning).toEqual({ bash: { long_running_reminder_enabled: false } });
      const empty: Record<string, unknown> = { experimental: { bash: {} } };
      expect(translateConfigDocument(empty, tier).retiredKeys).toEqual(["experimental.bash"]);
      expect(empty).toEqual({});
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

  test("a retired bridge.hang_threshold is dropped and the notice says it does nothing", () => {
    const doc: Record<string, unknown> = {
      bridge: { hang_threshold: 3, request_timeout_ms: 45_000 },
      harnesses: { pi: { bridge: { hang_threshold: 5 } } },
    };
    const out = translateConfigDocument(doc, "project");
    expect(doc).toEqual({ bridge: { request_timeout_ms: 45_000 }, harnesses: { pi: {} } });
    expect(out.legacyInput).toBe(true);
    expect(out.retiredKeys).toEqual([
      "bridge.hang_threshold",
      "harnesses.pi.bridge.hang_threshold",
    ]);
    // Nothing is applied for a key that no longer does anything, so a file
    // whose only retired keys are such keys gets just their sentence.
    expect(legacyConfigNoticeMessage("/repo/.cortexkit/aft.jsonc", out)).toBe(
      "bridge.hang_threshold no longer does anything: a timed-out request no longer restarts AFT, so the setting can be removed.",
    );
    const inert = translateConfigDocument(
      { inspect: { tier2_soft_deadline_ms: 50, max_drill_down_items: 20 } },
      "project",
    );
    expect(legacyConfigNoticeMessage("/p", inert)).toBe(
      "inspect.max_drill_down_items never had an effect, so the setting can be removed. inspect.tier2_soft_deadline_ms never had an effect, so the setting can be removed.",
    );

    // Mixed with keys that have equivalents: the generic sentence names only
    // those keys, and the no-effect sentence follows.
    const mixed = translateConfigDocument(
      {
        search_index: false,
        bridge: { hang_threshold: 3 },
        harnesses: { pi: { bridge: { hang_threshold: 5 } } },
      },
      "project",
    );
    expect(legacyConfigNoticeMessage("/repo/.cortexkit/aft.jsonc", mixed)).toBe(
      `/repo/.cortexkit/aft.jsonc uses retired keys (search_index); AFT applied their current equivalents, with the same limits a project config has for those keys. Run \`npx @cortexkit/aft doctor --fix\` to update the file. ${RETIRED_HANG_THRESHOLD_NOTE}`,
    );
    const withEnabled = translateConfigDocument(
      { enabled: false, bridge: { hang_threshold: 3 } },
      "project",
    );
    expect(
      legacyConfigNoticeMessage("/p", withEnabled).endsWith(
        `${RETIRED_ENABLED_FALSE_INDEXES_NOTE} ${RETIRED_HANG_THRESHOLD_NOTE}`,
      ),
    ).toBe(true);

    const only: Record<string, unknown> = { bridge: { hang_threshold: 2 } };
    translateConfigDocument(only, "user");
    expect(only).toEqual({});
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

describe("data-layer keys", () => {
  test("release parsing reads major and minor, and the running release is this package's", () => {
    expect(parseRelease("0.65.0")).toEqual([0, 65]);
    expect(parseRelease("0.64.12-beta.1")).toEqual([0, 64]);
    expect(parseRelease("latest")).toBeUndefined();
    const manifest = JSON.parse(
      readFileSync(new URL("../../package.json", import.meta.url), "utf8"),
    ) as { version: string };
    expect(getRunningRelease()).toEqual(parseRelease(manifest.version) ?? [0, 0]);
  });

  test("the idle-checkout rename keeps its existing release-gated refusal", () => {
    expect(DATA_LAYER_KEYS_REFUSED_FROM).toEqual([0, 65]);
    for (const path of DATA_LAYER_RETIRED_PATHS) {
      const [container = "", leaf = ""] = path.split(".");
      const base = { [container]: { [leaf]: 1 } };
      const harness = { harnesses: { pi: { [container]: { [leaf]: 1 } } } };
      expect(removedConfigKeyErrors(base, [0, 64]), path).toEqual([]);
      expect(removedConfigKeyErrors(harness, [0, 64]), path).toEqual([]);
      expect(removedConfigKeyErrors(base, [0, 65])).toEqual([
        `removed_config_key:${path}; run npx @cortexkit/aft doctor --fix`,
      ]);
      expect(removedConfigKeyErrors(harness, [0, 65])).toEqual([
        `removed_config_key:harnesses.pi.${path}; run npx @cortexkit/aft doctor --fix`,
      ]);
      expect(removedConfigKeyErrors(base, [1, 0])).toHaveLength(1);
      expect(removedConfigKeyErrors(base, [0, 65], false)).toEqual([]);
      expect(removedConfigKeyErrors(harness, [0, 65], false)).toEqual([]);
    }
  });

  test("removed no-effect keys refuse user config but not project config, including harness blocks", () => {
    for (const path of ["views.enabled", "views.callgraph_copies", "worktree.ram_overlay"]) {
      const [container = "", leaf = ""] = path.split(".");
      for (const [doc, prefix] of [
        [{ [container]: { [leaf]: false } }, ""],
        [{ harnesses: { pi: { [container]: { [leaf]: false } } } }, "harnesses.pi."],
      ] as const) {
        expect(removedConfigKeyErrors(doc, [0, 65])).toEqual([
          `removed_config_key:${prefix}${path}; run npx @cortexkit/aft doctor --fix`,
        ]);
        expect(removedConfigKeyErrors(doc, [0, 65], false)).toEqual([]);
      }
    }
    expect(removedConfigKeyErrors({ inspect: { tier2_idle_minutes: 5 } }, [0, 65])).toEqual([]);
  });

  test("views.idle_checkout_hours becomes idle.checkout_hours, clamped, unless that is set", () => {
    const doc: Record<string, unknown> = {
      views: { idle_checkout_hours: 48 },
      idle: { root_ttl_minutes: 20 },
      harnesses: { pi: { views: { idle_checkout_hours: 0, enabled: true } } },
    };
    const out = translateConfigDocument(doc, "user");
    expect(doc).toEqual({
      idle: { root_ttl_minutes: 20, checkout_hours: 48 },
      harnesses: { pi: { idle: { checkout_hours: 1 } } },
    });
    expect(out.legacyInput).toBe(true);
    expect(out.retiredKeys).toEqual([
      "harnesses.pi.views.enabled",
      "harnesses.pi.views.idle_checkout_hours",
      "views.idle_checkout_hours",
    ]);
    expect([9000, 12.0, 12.5, "never"].map(retiredIdleCheckoutHours)).toEqual([8760, 12, 168, 168]);

    const canonical: Record<string, unknown> = {
      views: { idle_checkout_hours: 2 },
      idle: { checkout_hours: 5 },
    };
    const superseded = translateConfigDocument(canonical, "project");
    expect(canonical).toEqual({ idle: { checkout_hours: 5 } });
    expect(superseded.warnings[0]).toMatchObject({
      code: "superseded_legacy_config",
      message: "views.idle_checkout_hours is ignored because idle.checkout_hours is set",
    });
  });

  test("removed keys are dropped before strict schema validation and listed as retired", () => {
    const doc = {
      views: { enabled: true, callgraph_copies: 2 },
      worktree: { ram_overlay: false },
      harnesses: { opencode: { worktree: { ram_overlay: true } } },
    };
    const translated = structuredClone(doc) as Record<string, unknown>;
    const out = translateConfigDocument(translated, "user");
    expect(translated).toEqual({ harnesses: { opencode: {} } });
    expect(out.legacyInput).toBe(true);
    expect(out.retiredKeys).toEqual([
      "harnesses.opencode.worktree.ram_overlay",
      "views.callgraph_copies",
      "views.enabled",
      "worktree.ram_overlay",
    ]);
    // The doctor preview remains idempotent after load-time translation.
    dropRemovedDataLayerKeys(translated);
    expect(translated).toEqual({ harnesses: { opencode: {} } });
  });

  test("project notices reuse existing no-effect sentences without promising refusal", () => {
    const removed = translateConfigDocument(
      { views: { enabled: true, callgraph_copies: 2 }, worktree: { ram_overlay: false } },
      "user",
    );
    const reasons = REMOVED_DATA_LAYER_NOTES.map(([, note]) => note).join(" ");
    expect(legacyConfigNoticeMessage("/p", removed)).toBe(reasons);

    const one = translateConfigDocument(
      { harnesses: { pi: { worktree: { ram_overlay: true } } } },
      "project",
    );
    expect(legacyConfigNoticeMessage("/p", one)).toBe(REMOVED_DATA_LAYER_NOTES[2]?.[1] ?? "");

    const both = translateConfigDocument(
      { views: { idle_checkout_hours: 24, enabled: true } },
      "project",
    );
    const idleNote =
      "views.idle_checkout_hours is now idle.checkout_hours; from 0.65.0 AFT refuses a config file that still sets views.idle_checkout_hours.";
    expect(retiredIdleCheckoutHoursNote()).toBe(idleNote);
    expect(legacyConfigNoticeMessage("/p", both)).toBe(
      `/p uses retired keys (views.idle_checkout_hours); AFT applied their current equivalents, with the same limits a project config has for those keys. Run \`npx @cortexkit/aft doctor --fix\` to update the file. ${idleNote} ${REMOVED_DATA_LAYER_NOTES[1]?.[1]}`,
    );
  });

  test("notice identities change when a data-layer key is added", () => {
    const digest = (doc: Record<string, unknown>) => noticeDigest(noticeProjection(doc));
    const acknowledged = digest({ search_index: false });
    for (const path of DATA_LAYER_RETIRED_PATHS) {
      const [container = "", leaf = ""] = path.split(".");
      expect(digest({ search_index: false, [container]: { [leaf]: 1 } }), path).not.toBe(
        acknowledged,
      );
    }
  });
});

describe("migration notice delivery", () => {
  test("a project that acknowledged one retired key is notified again when it adds bridge.hang_threshold", () => {
    const root = mkdtempSync(join(tmpdir(), "aft-notice-hang-"));
    roots.push(root);
    const storePath = join(root, "state", "migration-notices.json");
    const configPath = join(root, ".cortexkit", "aft.jsonc");
    const delivered: string[] = [];
    // The plugin loaders build the digest from the raw document and the
    // message from its translation, exactly like this.
    const load = (doc: Record<string, unknown>): boolean =>
      deliverMigrationNoticeOnce({
        configPath,
        digest: noticeDigest(noticeProjection(structuredClone(doc))),
        message: legacyConfigNoticeMessage(
          configPath,
          translateConfigDocument(structuredClone(doc), "project"),
        ),
        deliver: (message) => delivered.push(message),
        storePath,
      });

    expect(load({ search_index: false })).toBe(true);
    // Nothing that a notice depends on changed: no repeat.
    expect(load({ search_index: false })).toBe(false);
    expect(load({ search_index: false, bridge: { request_timeout_ms: 45_000 } })).toBe(false);
    // Adding the retired key is a new identity with a notice that names it.
    expect(load({ search_index: false, bridge: { hang_threshold: 3 } })).toBe(true);
    expect(load({ search_index: false, bridge: { hang_threshold: 3 } })).toBe(false);
    expect(
      load({ search_index: false, harnesses: { pi: { bridge: { hang_threshold: 5 } } } }),
    ).toBe(true);
    expect(delivered).toHaveLength(3);
    expect(delivered[1]).toBe(
      `${configPath} uses retired keys (search_index); AFT applied their current equivalents, with the same limits a project config has for those keys. Run \`npx @cortexkit/aft doctor --fix\` to update the file. ${RETIRED_HANG_THRESHOLD_NOTE}`,
    );
    expect(delivered[2]?.endsWith(RETIRED_HANG_THRESHOLD_NOTE)).toBe(true);
  });

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
