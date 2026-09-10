import { describe, expect, test } from "bun:test";

import type { PluginContext } from "../shared/types.js";
import { astTools } from "../tools/ast.js";
import { conflictTools } from "../tools/conflicts.js";
import { aftPrefixedTools, hoistedTools } from "../tools/hoisted.js";
import { importTools } from "../tools/imports.js";
import { inspectTools } from "../tools/inspect.js";
import { navigationTools } from "../tools/navigation.js";
import { readingTools } from "../tools/reading.js";
import { safetyTools } from "../tools/safety.js";
import { searchTools } from "../tools/search.js";
import { semanticTools } from "../tools/semantic.js";
import { buildWorkflowHints, HASHLINE_TAG_SOURCE_HINT } from "../workflow-hints.js";

// The agent-visible tool surface (names, descriptions, parameter schemas) and
// the injected system-prompt hints MUST be byte-identical between the
// standalone NDJSON transport and the subc daemon transport. Any divergence
// busts users' prefix caches on a transport flip and forks the wire the model
// sees. The surface must therefore be a pure function of config — never of
// the pool behind ctx. This test builds the full surface twice with ctx.pool
// swapped between two structurally different stand-ins and asserts deep
// equality of everything the model sees.

function fakePool(label: string): PluginContext["pool"] {
  // Distinct shapes on purpose: if any tool definition consults the pool while
  // BUILDING its description/schema, the two surfaces diverge and this test
  // fails. Execution-time pool use is out of scope (and transport-agnostic by
  // the AftTransportPool contract).
  return {
    label,
    getBridge: () => {
      throw new Error(`surface construction must not touch the pool (${label})`);
    },
    closeSession: () => {},
    shutdown: () => {},
    setConfigureOverride: () => {},
  } as unknown as PluginContext["pool"];
}

function buildSurface(pool: PluginContext["pool"]): Record<string, unknown> {
  const ctx = {
    pool,
    client: {} as PluginContext["client"],
    config: {
      tool_surface: "all",
      semantic_search: true,
      search_index: true,
    },
    storageDir: "/tmp/aft-surface-test",
    isProjectEnabled: () => true,
  } as unknown as PluginContext;

  const tools = {
    ...hoistedTools(ctx),
    ...aftPrefixedTools(ctx),
    ...readingTools(ctx),
    ...safetyTools(ctx),
    ...importTools(ctx),
    ...navigationTools(ctx),
    ...astTools(ctx),
    ...semanticTools(ctx),
    ...inspectTools(ctx),
    ...searchTools(ctx),
    ...conflictTools(ctx),
  };

  const surface: Record<string, unknown> = {};
  for (const [name, def] of Object.entries(tools)) {
    const definition = def as { description?: string; args?: unknown; parameters?: unknown };
    surface[name] = {
      description: definition.description,
      // Serialize schema shape; Zod objects stringify stably enough for
      // equality via JSON round-trip of their JSON-schema-ish own fields.
      args: JSON.parse(JSON.stringify(definition.args ?? definition.parameters ?? null)),
    };
  }
  return surface;
}

describe("tool surface transport invariance", () => {
  test("hashline edit description provides the patch grammar", () => {
    const ctx = {
      pool: fakePool("hashline-description"),
      client: {} as PluginContext["client"],
      config: { edit_mode: "hashline" },
      storageDir: "/tmp/aft-hashline-surface-test",
      hashlineEffective: true,
    } as PluginContext;
    const description = hoistedTools(ctx).edit?.description ?? "";

    for (const fragment of [
      "[path#TAG]",
      "current tagged read",
      "`N.=M`",
      "plain `N` PUT replaces",
      "`PUT <address>:`",
      "`+` alone is blank",
      "REM: bare `REM` only",
      "MV: `MV <destination>`",
    ]) {
      expect(description).toContain(fragment);
    }
  });

  test("the hashline edit description names the tools that mint tags", () => {
    const ctx = {
      pool: fakePool("hashline-tag-sources"),
      client: {} as PluginContext["client"],
      config: { edit_mode: "hashline" },
      storageDir: "/tmp/aft-hashline-tag-source-test",
      hashlineEffective: true,
    } as PluginContext;
    const description = hoistedTools(ctx).edit?.description ?? "";

    expect(description).toContain(
      "Only `read` (and accepted AFT `cat`/`head`/`tail` rewrites) mint hashline tags. `aft_zoom`, `aft_outline`, `grep`, `aft_search`, and conflict snippets do not. After navigation, call `read` on every file and range the patch addresses.",
    );
  });

  test("legacy edit users see byte-identical text", () => {
    // The routing rule is conditional on the hashline arm. A legacy session has
    // no tags at all, so being told to go mint them would be noise.
    const legacyCtx = {
      pool: fakePool("legacy-edit"),
      client: {} as PluginContext["client"],
      config: {},
      storageDir: "/tmp/aft-legacy-edit-test",
    } as PluginContext;
    const before =
      "Edit a file by finding and replacing text, or by targeting named symbols. To write or overwrite a whole file, use the `write` tool — `edit` requires an explicit edit mode and will not silently overwrite a file from `content` alone.";
    const description = hoistedTools(legacyCtx).edit?.description ?? "";
    expect(description.startsWith(before)).toBe(true);
    expect(description).not.toContain("mint hashline tags");

    const opts = {
      toolSurface: "all" as const,
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: true,
      bashCompressionEnabled: true,
      disabledTools: new Set<string>(),
    };
    const legacyHints = buildWorkflowHints(opts);
    expect(buildWorkflowHints({ ...opts, hashlineEffective: false })).toBe(legacyHints);
    expect(legacyHints).not.toContain("mint hashline tags");
    expect(buildWorkflowHints({ ...opts, hashlineEffective: true })).toBe(
      `${legacyHints}\n\n${HASHLINE_TAG_SOURCE_HINT}`,
    );
  });

  test("tool names, descriptions, and schemas are independent of the pool", () => {
    const standalone = buildSurface(fakePool("standalone-ndjson"));
    const subc = buildSurface(fakePool("subc-daemon"));

    expect(Object.keys(subc).sort()).toEqual(Object.keys(standalone).sort());
    expect(subc).toEqual(standalone);
  });

  test("workflow hints are a pure function of config, not transport", () => {
    const opts = {
      toolSurface: "all" as const,
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: true,
      bashCompressionEnabled: true,
      disabledTools: new Set<string>(),
    };
    const first = buildWorkflowHints(opts);
    const second = buildWorkflowHints(opts);
    expect(second).toEqual(first);
    expect(typeof first).toBe("string");
    expect((first as string).length).toBeGreaterThan(0);
    // Nothing transport-shaped may appear in the injected prompt text.
    expect(first as string).not.toMatch(/subc|ndjson|daemon|transport/i);
  });
});

describe("conditional GitHub mutation descriptions", () => {
  test("write and edit advertise resource forms only when github.write is effective", () => {
    const base = {
      pool: fakePool("github-description"),
      client: {} as PluginContext["client"],
      storageDir: "/tmp/aft-github-description-test",
    };
    const enabled = hoistedTools({
      ...base,
      config: { github: { write: true } },
    } as PluginContext);
    expect(enabled.write.description).toContain('write("issue://N", content)');
    expect(enabled.edit.description).toContain('edit("issue://N/comments/K"');

    const disabled = hoistedTools({
      ...base,
      config: { github: { enabled: false, write: true } },
    } as PluginContext);
    expect(disabled.write.description).not.toContain("issue://N");
    expect(disabled.edit.description).not.toContain("issue://N/comments/K");
  });
});
