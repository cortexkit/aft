/**
 * The shell tool surface shown to a model must match the features that are on.
 *
 * Every parameter, companion tool and description sentence of the shell family
 * depends on one setting: `compressed` on `bash.compress`; `wait`,
 * `background`, the PTY parameters and the companion tools (`bash_status`,
 * `bash_watch`, `bash_write`, `bash_kill`) on `bash.background`; `sandbox` on
 * `sandbox.profile.head`. A model shown an option that does nothing, or a tool whose
 * description points at a parameter it cannot pass, wastes calls on it. This
 * file walks every combination of the three settings through the production
 * registration path of both plugins and checks the schema keys, the registered
 * names, and that no description or workflow hint names something absent.
 */
import { describe, expect, test } from "bun:test";
import type {
  ExtensionAPI,
  ToolDefinition as PiToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { tool } from "@opencode-ai/plugin";
import type { AftConfig as PiConfig } from "../../../../packages/pi-plugin/src/config.js";
import { MAGIC_CONTEXT_SUBAGENT_ENV } from "../../../../packages/pi-plugin/src/session-kind.js";
import {
  registerPiToolSurface,
  resolvePiToolSurface,
} from "../../../../packages/pi-plugin/src/tool-registration.js";
import type { PluginContext as PiContext } from "../../../../packages/pi-plugin/src/types.js";
import { registerWorkflowHints as registerPiWorkflowHints } from "../../../../packages/pi-plugin/src/workflow-hints.js";
import type { AftConfig as OpenCodeConfig } from "../config.js";
import { buildOpenCodeToolMap } from "../tool-registration.js";
import type { PluginContext as OpenCodeContext } from "../types.js";
import { buildHintsForRegisteredTools } from "../workflow-hints.js";

const COMPANIONS = ["bash_status", "bash_watch", "bash_write", "bash_kill"] as const;

type Combo = { compress: boolean; background: boolean; sandbox: boolean };

const COMBOS: Combo[] = [];
for (const compress of [true, false]) {
  for (const background of [true, false]) {
    for (const sandbox of [true, false]) COMBOS.push({ compress, background, sandbox });
  }
}

function label(combo: Combo): string {
  return `compress=${combo.compress ? "on" : "off"} background=${combo.background ? "on" : "off"} sandbox=${combo.sandbox ? "on" : "off"}`;
}

function configFor(combo: Combo): Record<string, unknown> {
  return {
    disabled_tools: [],
    bash: { compress: combo.compress, background: combo.background },
    ...(combo.sandbox ? { sandbox: { profile: { head: "strict" } } } : {}),
  };
}

function expectedBashKeys(combo: Combo): string[] {
  const keys = ["command", "timeout", "workdir", "description", "escape_sandbox"];
  if (combo.background) keys.push("wait", "background", "pty", "ptyRows", "ptyCols");
  if (combo.compress) keys.push("compressed");
  if (combo.sandbox) keys.push("sandbox");
  return keys.sort();
}

/**
 * Text patterns that name a parameter or behaviour. Each is checked only when
 * its feature is off, so wording that stays while the feature is on is free.
 */
function forbiddenPatterns(combo: Combo): Array<{ what: string; pattern: RegExp }> {
  const forbidden: Array<{ what: string; pattern: RegExp }> = [];
  if (!combo.compress) forbidden.push({ what: "compressed", pattern: /compress/i });
  if (!combo.background) {
    forbidden.push(
      { what: "wait", pattern: /\bwait\s*:|`wait`/ },
      { what: "background", pattern: /\bbackground\b/i },
      { what: "pty", pattern: /\bpty\b/i },
      { what: "auto-promotion", pattern: /promot/i },
      { what: "wait detachment", pattern: /detach/i },
    );
  }
  // The escape reason is always present, including at Off, where it is a no-op.
  // Schema keys above still pin the conditional `sandbox` switch itself.
  return forbidden;
}

/**
 * Hints also carry unrelated prose (for example an inspect refresh described
 * as a "background run"), so only parameter-shaped mentions and the gated
 * section headings are checked there.
 */
function forbiddenHintPatterns(combo: Combo): Array<{ what: string; pattern: RegExp }> {
  const forbidden: Array<{ what: string; pattern: RegExp }> = [];
  if (!combo.compress) forbidden.push({ what: "compressed", pattern: /compress/i });
  if (!combo.background) {
    forbidden.push(
      { what: "wait", pattern: /\bwait\s*:/ },
      { what: "background", pattern: /\bbackground\s*:/ },
      { what: "pty", pattern: /\bpty\b/i },
    );
  }
  if (!combo.sandbox) forbidden.push({ what: "sandbox", pattern: /\bsandbox/i });
  return forbidden;
}

/** Collect every `description` string inside a JSON Schema. */
function schemaDescriptions(value: unknown, out: string[] = []): string[] {
  if (Array.isArray(value)) {
    for (const item of value) schemaDescriptions(item, out);
  } else if (value && typeof value === "object") {
    for (const [key, child] of Object.entries(value as Record<string, unknown>)) {
      if (key === "description" && typeof child === "string") out.push(child);
      else schemaDescriptions(child, out);
    }
  }
  return out;
}

function assertNoAbsentMentions(
  texts: Array<{ where: string; text: string }>,
  registered: ReadonlySet<string>,
  forbidden: Array<{ what: string; pattern: RegExp }>,
): void {
  for (const { where, text } of texts) {
    for (const companion of COMPANIONS) {
      if (registered.has(companion)) continue;
      expect(
        new RegExp(`\\b${companion}\\b`).test(text),
        `${where} names unregistered tool ${companion}`,
      ).toBe(false);
    }
    for (const { what, pattern } of forbidden) {
      expect(pattern.test(text), `${where} mentions absent ${what}: ${text}`).toBe(false);
    }
  }
}

function stubContext(config: Record<string, unknown>): OpenCodeContext & PiContext {
  const pool = {
    getBridge: () => {
      throw new Error("surface construction must not touch the bridge");
    },
  };
  return { pool, config, storageDir: "/tmp/aft-shell-surface-matrix" } as never;
}

function openCodeSurface(combo: Combo) {
  const config = configFor(combo);
  const tools = buildOpenCodeToolMap(
    stubContext(config) as OpenCodeContext,
    config as OpenCodeConfig,
  ) as Record<string, { description?: string; args: Record<string, unknown> }>;
  return { config, tools, registered: new Set(Object.keys(tools)) };
}

describe("OpenCode shell surface follows bash.compress, bash.background and sandbox.profile.head", () => {
  for (const combo of COMBOS) {
    test(`${label(combo)}: bash schema keys`, () => {
      const { tools } = openCodeSurface(combo);
      expect(Object.keys(tools.bash?.args ?? {}).sort()).toEqual(expectedBashKeys(combo));
    });

    test(`${label(combo)}: registered companions`, () => {
      const { registered } = openCodeSurface(combo);
      for (const companion of COMPANIONS) {
        expect(registered.has(companion), companion).toBe(combo.background);
      }
    });

    test(`${label(combo)}: descriptions and hints name only what is present`, () => {
      const { config, tools, registered } = openCodeSurface(combo);
      const texts: Array<{ where: string; text: string }> = [];
      for (const name of ["bash", ...COMPANIONS]) {
        const definition = tools[name];
        if (!definition) continue;
        texts.push({ where: `${name} description`, text: definition.description ?? "" });
        const schema = tool.schema.toJSONSchema(tool.schema.object(definition.args), {
          io: "input",
        });
        for (const text of schemaDescriptions(schema)) {
          texts.push({ where: `${name} parameter`, text });
        }
      }
      assertNoAbsentMentions(texts, registered, forbiddenPatterns(combo));

      const hints = buildHintsForRegisteredTools(config as OpenCodeConfig, registered) ?? "";
      assertNoAbsentMentions(
        [{ where: "workflow hints", text: hints }],
        registered,
        forbiddenHintPatterns(combo),
      );
    });
  }
});

function capturePi(
  config: Record<string, unknown>,
  hashlineEffective = false,
): {
  tools: Map<string, PiToolDefinition>;
  hints: string;
} {
  const tools = new Map<string, PiToolDefinition>();
  const handlers: Array<(event: { systemPrompt: string }) => { systemPrompt: string }> = [];
  const pi = {
    registerTool(definition: PiToolDefinition) {
      tools.set(definition.name, definition);
    },
    on(_event: string, handler: (event: { systemPrompt: string }) => { systemPrompt: string }) {
      handlers.push(handler);
    },
  } as unknown as ExtensionAPI;
  const ctx = stubContext(config) as PiContext;
  ctx.hashlineEffective = hashlineEffective;
  const surface = resolvePiToolSurface(ctx.config as PiConfig);
  registerPiToolSurface(pi, ctx, surface);

  // Hint registration is skipped for delegated Magic Context children; clear
  // the marker so this check always sees the block a primary session gets.
  const previous = process.env[MAGIC_CONTEXT_SUBAGENT_ENV];
  delete process.env[MAGIC_CONTEXT_SUBAGENT_ENV];
  try {
    registerPiWorkflowHints(pi, ctx.config as PiConfig, surface);
  } finally {
    if (previous !== undefined) process.env[MAGIC_CONTEXT_SUBAGENT_ENV] = previous;
  }
  const hints = handlers[0]?.({ systemPrompt: "" }).systemPrompt ?? "";
  return { tools, hints };
}

describe("Pi shell surface follows bash.compress, bash.background and sandbox.profile.head", () => {
  for (const combo of COMBOS) {
    test(`${label(combo)}: bash schema keys`, () => {
      const { tools } = capturePi(configFor(combo));
      const bash = tools.get("bash") as
        | (PiToolDefinition & { parameters?: { properties?: Record<string, unknown> } })
        | undefined;
      expect(Object.keys(bash?.parameters?.properties ?? {}).sort()).toEqual(
        expectedBashKeys(combo),
      );
    });

    test(`${label(combo)}: registered companions`, () => {
      const { tools } = capturePi(configFor(combo));
      for (const companion of COMPANIONS) {
        expect(tools.has(companion), companion).toBe(combo.background);
      }
    });

    test(`${label(combo)}: descriptions and hints name only what is present`, () => {
      const { tools, hints } = capturePi(configFor(combo));
      const registered = new Set(tools.keys());
      const texts: Array<{ where: string; text: string }> = [];
      for (const name of ["bash", ...COMPANIONS]) {
        const definition = tools.get(name) as
          | (PiToolDefinition & { promptSnippet?: string; promptGuidelines?: string[] })
          | undefined;
        if (!definition) continue;
        texts.push({ where: `${name} description`, text: definition.description ?? "" });
        texts.push({ where: `${name} promptSnippet`, text: definition.promptSnippet ?? "" });
        for (const guideline of definition.promptGuidelines ?? []) {
          texts.push({ where: `${name} promptGuidelines`, text: guideline });
        }
        for (const text of schemaDescriptions(definition.parameters)) {
          texts.push({ where: `${name} parameter`, text });
        }
      }
      assertNoAbsentMentions(texts, registered, forbiddenPatterns(combo));
      assertNoAbsentMentions(
        [{ where: "workflow hints", text: hints }],
        registered,
        forbiddenHintPatterns(combo),
      );
    });
  }
});

/**
 * Tools named in `disabled_tools` are not registered, so no description,
 * parameter text, prompt snippet or workflow hint may name them either. Every
 * registered tool's text is scanned, not only the shell family, because other
 * tools point at each other too (for example "prefer aft_search + aft_zoom").
 * Only AFT's own names are checked: `bash`, `read` and `grep` are host slots,
 * and disabling AFT's registration leaves the host's own tool under that name.
 */
const DISABLED_CASES: string[][] = [
  [],
  ["bash_status"],
  ["bash_watch"],
  ["bash_write"],
  ["bash_kill"],
  ["aft_search"],
  ["aft_zoom"],
  ["aft_outline"],
  ["aft_search", "aft_zoom"],
  ["aft_callgraph"],
];

const AFT_TOOL_NAME = /\b(aft_[a-z_]+|bash_(?:status|watch|write|kill))\b/g;

function disabledConfig(disabled: string[], hashline: boolean): Record<string, unknown> {
  return {
    disabled_tools: disabled,
    sandbox: { profile: { head: "strict" } },
    ...(hashline ? { edit_mode: "hashline" } : {}),
  };
}

function disabledLabel(disabled: string[], hashline: boolean): string {
  const list = disabled.length > 0 ? disabled.join("+") : "nothing";
  return `disabled=${list} hashline=${hashline ? "on" : "off"}`;
}

function assertNamesRegistered(
  texts: Array<{ where: string; text: string }>,
  registered: ReadonlySet<string>,
): void {
  // Collect every violation so one run shows all of them, not just the first.
  const violations = new Set<string>();
  for (const { where, text } of texts) {
    for (const match of text.matchAll(AFT_TOOL_NAME)) {
      if (!registered.has(match[1])) violations.add(`${where} names ${match[1]}`);
    }
  }
  expect([...violations]).toEqual([]);
}

describe("OpenCode text names only registered tools when tools are disabled", () => {
  for (const disabled of DISABLED_CASES) {
    for (const hashline of [false, true]) {
      test(disabledLabel(disabled, hashline), () => {
        const config = disabledConfig(disabled, hashline);
        const ctx = stubContext(config) as OpenCodeContext;
        ctx.hashlineEffective = hashline;
        const tools = buildOpenCodeToolMap(ctx, config as OpenCodeConfig) as Record<
          string,
          { description?: string; args: Record<string, unknown> }
        >;
        const registered = new Set(Object.keys(tools));
        for (const name of disabled) expect(registered.has(name), name).toBe(false);

        const texts: Array<{ where: string; text: string }> = [];
        for (const [name, definition] of Object.entries(tools)) {
          texts.push({ where: `${name} description`, text: definition.description ?? "" });
          const schema = tool.schema.toJSONSchema(tool.schema.object(definition.args), {
            io: "input",
          });
          for (const text of schemaDescriptions(schema)) {
            texts.push({ where: `${name} parameter`, text });
          }
        }
        texts.push({
          where: "workflow hints",
          text: buildHintsForRegisteredTools(config as OpenCodeConfig, registered, hashline) ?? "",
        });
        assertNamesRegistered(texts, registered);
      });
    }
  }
});

describe("Pi text names only registered tools when tools are disabled", () => {
  for (const disabled of DISABLED_CASES) {
    for (const hashline of [false, true]) {
      test(disabledLabel(disabled, hashline), () => {
        const { tools, hints } = capturePi(disabledConfig(disabled, hashline), hashline);
        const registered = new Set(tools.keys());
        for (const name of disabled) expect(registered.has(name), name).toBe(false);

        const texts: Array<{ where: string; text: string }> = [];
        for (const [name, definition] of tools) {
          const pi = definition as PiToolDefinition & {
            promptSnippet?: string;
            promptGuidelines?: string[];
          };
          texts.push({ where: `${name} description`, text: pi.description ?? "" });
          texts.push({ where: `${name} promptSnippet`, text: pi.promptSnippet ?? "" });
          for (const guideline of pi.promptGuidelines ?? []) {
            texts.push({ where: `${name} promptGuidelines`, text: guideline });
          }
          for (const text of schemaDescriptions(pi.parameters)) {
            texts.push({ where: `${name} parameter`, text });
          }
        }
        texts.push({ where: "workflow hints", text: hints });
        assertNamesRegistered(texts, registered);
      });
    }
  }
});
