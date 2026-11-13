/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import type { AftConfig } from "../config.js";
import { MAGIC_CONTEXT_SUBAGENT_ENV } from "../session-kind.js";
import {
  applyWorkflowHints,
  buildHintsFromConfig,
  buildWorkflowHints,
  HASHLINE_TAG_SOURCE_HINT,
  registerWorkflowHints,
  WORKFLOW_HINTS_SECTION,
  type WorkflowHintsStartEvent,
  type WorkflowHintsStartResult,
} from "../workflow-hints.js";

describe("Pi hashline tag-source guidance", () => {
  const opts = {
    bashBackgroundEnabled: true,
    bashCompressionEnabled: true,
    absentTools: new Set<string>(),
  };

  test("legacy edit users see byte-identical text", () => {
    const legacy = buildWorkflowHints(opts);
    expect(buildWorkflowHints({ ...opts, hashlineEffective: false })).toBe(legacy);
    expect(legacy).not.toContain("mint hashline tags");
  });

  test("hashline sessions are told which tools mint tags", () => {
    const legacy = buildWorkflowHints(opts);
    expect(buildWorkflowHints({ ...opts, hashlineEffective: true })).toBe(
      `${legacy}\n\n${HASHLINE_TAG_SOURCE_HINT}`,
    );
    expect(HASHLINE_TAG_SOURCE_HINT).toContain(
      "Only `read` (and accepted AFT `cat`/`head`/`tail` rewrites) mint hashline tags. `aft_zoom`, `aft_outline`, `grep`, `aft_search`, and conflict snippets do not. After navigation, call `read` on every file and range the patch addresses.",
    );
  });
});

describe("Pi buildWorkflowHints", () => {
  test("omits unavailable navigation tools without fallback wording", () => {
    const out = buildWorkflowHints({
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      absentTools: new Set(["aft_zoom"]),
    });
    expect(out).not.toContain("aft_zoom");
    expect(out).not.toContain("Then aft_outline");
    expect(out).not.toContain("**Web pages**");
    expect(out).toContain("start with aft_search;");
  });

  test("returns null when only the safety tool is registered", () => {
    expect(
      buildWorkflowHints({
        bashBackgroundEnabled: false,
        bashCompressionEnabled: false,
        absentTools: new Set([
          "aft_outline",
          "aft_zoom",
          "aft_search",
          "aft_callgraph",
          "aft_inspect",
          "grep",
          "read",
          "bash",
          "bash_status",
        ]),
      }),
    ).toBeNull();
  });
});

describe("Pi registerWorkflowHints", () => {
  test("appends hints normally but registers no hook in a Magic Context child", () => {
    const previous = process.env[MAGIC_CONTEXT_SUBAGENT_ENV];
    const config: AftConfig = {};
    const surface = {
      outline: true,
      zoom: true,
      semantic: true,
      navigate: true,
      inspect: true,
      hoistGrep: true,
      hoistBash: true,
      hoistEdit: true,
      hoistRead: true,
      bashStatus: true,
      bashWatch: true,
      bashWrite: true,
    };
    const handlers: Array<(event: { systemPrompt: string }) => { systemPrompt: string }> = [];
    const pi = {
      on: (
        event: string,
        handler: (event: { systemPrompt: string }) => { systemPrompt: string },
      ) => {
        expect(event).toBe("before_agent_start");
        handlers.push(handler);
      },
    } as unknown as ExtensionAPI;

    try {
      delete process.env[MAGIC_CONTEXT_SUBAGENT_ENV];
      registerWorkflowHints(pi, config, surface);
      expect(handlers).toHaveLength(1);
      const hints = buildHintsFromConfig(config, new Set());
      expect(hints).not.toBeNull();
      expect(handlers[0]?.({ systemPrompt: "existing prompt" })).toEqual({
        systemPrompt: `existing prompt\n\n${hints}`,
      });

      process.env[MAGIC_CONTEXT_SUBAGENT_ENV] = "1";
      registerWorkflowHints(pi, config, surface);
      expect(handlers).toHaveLength(1);
    } finally {
      if (previous === undefined) delete process.env[MAGIC_CONTEXT_SUBAGENT_ENV];
      else process.env[MAGIC_CONTEXT_SUBAGENT_ENV] = previous;
    }
  });
});

describe("Pi applyWorkflowHints across hosts", () => {
  const hints = buildHintsFromConfig({}, new Set()) ?? "";

  test("a host with prompt sections gets a section and no forced prompt", () => {
    const sections: Record<string, string> = { other_extension: "kept" };
    const event = {
      systemPrompt: "rendered prompt",
      systemPromptOptions: { sections },
    };
    expect(applyWorkflowHints(event, hints)).toBeUndefined();
    expect(WORKFLOW_HINTS_SECTION).toMatch(/^[a-z][a-z0-9_-]*$/);
    expect(sections).toEqual({ other_extension: "kept", [WORKFLOW_HINTS_SECTION]: hints });
  });

  test("a run an earlier handler already forced gets the hints appended to the forced text", () => {
    const sections: Record<string, string> = {};
    const event = {
      systemPrompt: "forced by another extension",
      systemPromptOptions: { sections, forceSystemPrompt: "forced by another extension" },
    };
    expect(applyWorkflowHints(event, hints)).toEqual({
      systemPrompt: `forced by another extension\n\n${hints}`,
    });
    expect(sections[WORKFLOW_HINTS_SECTION]).toBe(hints);
  });

  test("an array prompt keeps every block and gains the hints as its last block", () => {
    const blocks = ["first block, with a comma", "second block"];
    const result = applyWorkflowHints({ systemPrompt: blocks }, hints);
    expect(result).toEqual({ systemPrompt: [...blocks, hints] });
    const returned = result?.systemPrompt as string[];
    // No block may hold a comma-joined copy of the blocks, which is what
    // interpolating the array into a template string produces.
    expect(returned.some((block) => block.includes(blocks.join(",")))).toBe(false);
    expect(blocks).toEqual(["first block, with a comma", "second block"]);
  });

  test("a string prompt without sections is appended to as before", () => {
    expect(applyWorkflowHints({ systemPrompt: "existing prompt" }, hints)).toEqual({
      systemPrompt: `existing prompt\n\n${hints}`,
    });
  });

  test("older Pi options without sections are left untouched", () => {
    const systemPromptOptions = {
      cwd: "/project",
    } as WorkflowHintsStartEvent["systemPromptOptions"];
    expect(
      applyWorkflowHints({ systemPrompt: "existing prompt", systemPromptOptions }, hints),
    ).toEqual({ systemPrompt: `existing prompt\n\n${hints}` });
    expect(systemPromptOptions).toEqual({ cwd: "/project" } as typeof systemPromptOptions);
  });

  test("the registered handler writes byte-identical section text on every run", () => {
    const previous = process.env[MAGIC_CONTEXT_SUBAGENT_ENV];
    const handlers: Array<(event: WorkflowHintsStartEvent) => WorkflowHintsStartResult> = [];
    const pi = {
      on: (
        _event: string,
        handler: (event: WorkflowHintsStartEvent) => WorkflowHintsStartResult,
      ) => {
        handlers.push(handler);
      },
    } as unknown as ExtensionAPI;
    try {
      delete process.env[MAGIC_CONTEXT_SUBAGENT_ENV];
      registerWorkflowHints(
        pi,
        {},
        {
          outline: true,
          zoom: true,
          semantic: true,
          navigate: true,
          inspect: true,
          hoistGrep: true,
          hoistBash: true,
          hoistEdit: true,
          hoistRead: true,
          bashStatus: true,
          bashWatch: true,
          bashWrite: true,
        },
      );
      // Pi hands each run a fresh copy of the sections.
      const runs = [0, 1, 2].map(() => {
        const sections: Record<string, string> = {};
        expect(
          handlers[0]?.({ systemPrompt: "", systemPromptOptions: { sections } }),
        ).toBeUndefined();
        return sections[WORKFLOW_HINTS_SECTION];
      });
      expect(runs[0]).toBe(hints);
      expect(runs[1]).toBe(runs[0]);
      expect(runs[2]).toBe(runs[0]);
    } finally {
      if (previous === undefined) delete process.env[MAGIC_CONTEXT_SUBAGENT_ENV];
      else process.env[MAGIC_CONTEXT_SUBAGENT_ENV] = previous;
    }
  });
});

describe("Pi buildHintsFromConfig", () => {
  test("emits hints by default and includes hoisted bash name", () => {
    const config: AftConfig = {
      bash: { background: true },
    };
    const out = buildHintsFromConfig(config, new Set());
    expect(out).not.toBeNull();
    // The code-finding warning names the registered bash tool, not aft_bash.
    expect(out).toContain("Don't find or read code with grep, find, sed or cat in bash.");
    expect(out).toContain("**After edits**: run aft_inspect for diagnostics;");
  });

  test("never names the removed prefixed host tools", () => {
    const out = buildHintsFromConfig({ bash: true }, new Set());
    expect(out).not.toContain("`aft_read");
    expect(out).not.toContain("`aft_bash");
  });
});
