/// <reference path="../bun-test.d.ts" />

import { describe, expect, spyOn, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { TSchema } from "typebox";
import { Value } from "typebox/value";
import * as logger from "../logger.js";
import { registerPiToolSurface, resolvePiToolSurface } from "../tool-registration.js";
import { makeContext, mockTheme, renderToString } from "./render-test-helpers.js";
import {
  executeTool,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

// OMP 18.6.0 registers all these schemes, including mcp. MCP write refusal
// belongs to the native tool, not to the adapter's command-token predicate.
const schemes = new Set(
  "skill rule memory agent history artifact local proc cfg ssh security vault issue pr mcp omp xd attachment conflict".split(
    " ",
  ),
);
const writable = new Set("agent local proc cfg ssh vault xd conflict".split(" "));
const router = {
  normalize: (input: string) => input.replace(/^skill:\/(?!\/)/, "skill://"),
  canHandle(input: string) {
    return schemes.has(/^([a-z]+):\/\//i.exec(this.normalize(input))?.[1]?.toLowerCase() ?? "");
  },
  canResolve(input: string) {
    return this.canHandle(input) || /^(?:custom:\/\/|urn:)/.test(input);
  },
  writeTarget(input: string) {
    const scheme = /^([a-z]+):\/\//i.exec(this.normalize(input))?.[1]?.toLowerCase();
    return scheme && schemes.has(scheme)
      ? { spec: { write: writable.has(scheme) ? { via: "handler" } : undefined } }
      : undefined;
  },
};

function setup(
  harness: "omp" | "pi" = "omp",
  withRouter = true,
  hashline = false,
  cwd?: string,
  restrictToProjectRoot = false,
) {
  const { api, tools } = makeMockApi();
  const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "AFT", output: "AFT" }));
  const config = {
    disabled_tools: [],
    bash: true,
    github: { read: true, write: true },
    restrict_to_project_root: restrictToProjectRoot,
  };
  const ctx = makePluginContext(bridge, {
    config,
    hashlineEffective: hashline,
    ...{ ompRouter: withRouter ? router : undefined },
  });
  registerPiToolSurface(api, ctx, resolvePiToolSurface(config), harness);
  const invocations: unknown[] = [];
  const nativeResult = {
    content: [{ type: "text", text: "native status" }],
    details: { proc: { status: "killed" }, bg_completions: {}, diff: {} },
  };
  const extCtx = Object.assign(makeExtContext(cwd), {
    invokeTool: async (params: unknown) => {
      invocations.push(params);
      return nativeResult;
    },
    hasUI: true,
    ui: { confirm: async () => true },
  }) as ExtensionContext;
  return { tools, calls, invocations, extCtx, nativeResult };
}

async function withFilesystemTool(
  name: "bash" | "edit" | "grep",
  run: (fixture: ReturnType<typeof setup> & { root: string }) => Promise<void>,
) {
  const root = mkdtempSync(join(homedir(), `aft-omp-${name}-`));
  try {
    mkdirSync(join(root, "src"));
    mkdirSync(join(root, "tests"));
    const file = join(root, "src/example.ts");
    writeFileSync(file, "before\n");
    await run({ ...setup("omp", true, false, root, name === "grep"), root });
    expect(readFileSync(file, "utf8")).toBe("before\n");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

describe("OMP internal URL hand-back", () => {
  test("bash filesystem route drops inert native defaults in preparation and direct execution", async () => {
    await withFilesystemTool("bash", async (s) => {
      const tool = s.tools.get("bash")!;
      const { prepareArguments } = tool as unknown as {
        prepareArguments(args: unknown): Record<string, unknown>;
      };
      const control = { command: "pwd", description: "Show the working directory", workdir: ".", timeout: 500 };
      const padded = {
        ...control,
        cwd: "",
        async: false,
        name: "",
        ready: { log: "", port: 0, host: "", timeout: 0 },
      };
      expect(Value.Check(tool.parameters as TSchema, padded)).toBe(true);
      expect(prepareArguments({ ...control, ready: {} })).toEqual(control);
      const prepared = prepareArguments(padded);
      expect(prepared).toEqual(control);
      await executeTool(tool, prepared, s.extCtx);
      await executeTool(tool, padded, s.extCtx);
      expect(s.calls).toHaveLength(2);
      for (const { command, params } of s.calls) {
        expect(command).toBe("bash");
        expect(params).toMatchObject(control);
        expect(params).not.toHaveProperty("cwd");
        expect(params.background).toBe(false);
      }

      for (const [field, value] of [
        ["cwd", "src"],
        ["async", true],
        ["name", "service"],
        ["ready", { log: "listening" }],
        ["ready", { port: 8080 }],
        ["ready", { host: "localhost" }],
        ["ready", { timeout: 500 }],
      ] as const) {
        const active = { ...padded, [field]: value };
        const refusal = `bash: invalid AFT filesystem arguments: ${field}.`;
        expect(() => prepareArguments(active)).toThrow(refusal);
        await expect(executeTool(tool, active, s.extCtx)).rejects.toThrow(refusal);
      }
      expect(s.calls).toHaveLength(2);
      expect(s.invocations).toHaveLength(0);

      const native = { ...padded, cwd: "local://" };
      expect(prepareArguments(native)).toBe(native);
      await executeTool(tool, native, s.extCtx);
      expect(s.invocations[0]).toBe(native);
      expect(s.calls).toHaveLength(2);
    });
  });

  test("routing predicate delegates read without preparing or copying parameters", async () => {
    const s = setup();
    const params = { path: "agent://worker", offset: 0, opaque: "keep" };
    expect(Value.Check(s.tools.get("read")!.parameters as TSchema, params)).toBe(true);
    const prepare = (
      s.tools.get("read")! as unknown as { prepareArguments: (args: unknown) => unknown }
    ).prepareArguments;
    expect(prepare(params)).toBe(params);
    const result = await executeTool(s.tools.get("read")!, params, s.extCtx);
    expect(result).toEqual({
      ...s.nativeResult,
      details: { ...s.nativeResult.details, delegatedTo: "omp", delegatedTarget: params.path },
    });
    expect(s.nativeResult.details).not.toHaveProperty("delegatedTo");
    expect(s.invocations[0]).toBe(params);
    expect(s.calls).toHaveLength(0);
  });

  test("content-less proc kill passes the OMP write schema and delegates", async () => {
    const s = setup();
    const params = { path: "proc://x/kill" };
    expect(Value.Check(s.tools.get("write")!.parameters as TSchema, params)).toBe(true);
    await executeTool(s.tools.get("write")!, params, s.extCtx);
    expect(s.invocations[0]).toBe(params);
    expect(s.calls).toHaveLength(0);
  });

  test("xd and cfg writes and edits use invokeTool only", async () => {
    const s = setup();
    for (const path of ["xd://device", "cfg://settings"]) {
      for (const name of ["write", "edit"]) {
        const params = { path, content: "value" };
        await executeTool(s.tools.get(name)!, params, s.extCtx);
        expect(s.invocations.at(-1)).toBe(params);
      }
    }
    expect(s.calls).toHaveLength(0);
  });

  test("read-side router covers every registered scheme and opaque MCP fallback", async () => {
    const s = setup();
    for (const path of [...schemes]
      .map((scheme) => `${scheme}://x`)
      .concat(["custom://resource", "urn:example:doc"])) {
      await executeTool(s.tools.get("read")!, { path }, s.extCtx);
      await executeTool(s.tools.get("grep")!, { path, pattern: "x" }, s.extCtx);
    }
    expect(s.invocations).toHaveLength((schemes.size + 2) * 2);
    expect(s.calls).toHaveLength(0);
  });

  test("grep delegates mixed search paths as one unmodified call", async () => {
    const s = setup();
    for (const path of [["src", "local://*.md"], "src,local://*.md", "src;cfg://settings"]) {
      const params = { path, pattern: "x", skip: null, case: true };
      expect(Value.Check(s.tools.get("grep")!.parameters as TSchema, params)).toBe(true);
      await executeTool(s.tools.get("grep")!, params, s.extCtx);
      expect(s.invocations.at(-1)).toBe(params);
    }
    expect(s.calls).toHaveLength(0);
  });

  test("bash delegates native cwd and AFT workdir URLs", async () => {
    const s = setup();
    for (const key of ["cwd", "workdir"]) {
      const params = {
        command: "pwd",
        description: "Print the current working directory",
        [key]: "skill://sample",
        timeout: 0.5,
      };
      expect(Value.Check(s.tools.get("bash")!.parameters as TSchema, params)).toBe(true);
      await executeTool(s.tools.get("bash")!, params, s.extCtx);
      expect(s.invocations.at(-1)).toBe(params);
    }
    expect(s.calls).toHaveLength(0);
  });

  test("bash tokenizer delegates quoted URLs, aliases and registered MCP", async () => {
    const s = setup();
    for (const command of [
      'cat "skill://sample"',
      "ls local://",
      "cat skill://sample",
      "cat 'skill:/sample'",
      "cat `skill://sample`",
      "cat mcp://x",
      "cmd > mcp://x",
      "cat\tlocal://x;pwd",
      "tool --opt=skill://x",
      'tool --opt="skill://x"',
      "echo $(cat skill://x)",
    ]) {
      const params = { command };
      await executeTool(s.tools.get("bash")!, params, s.extCtx);
      expect(s.invocations.at(-1)).toBe(params);
    }
    expect(s.calls).toHaveLength(0);
    expect((s.tools.get("bash") as unknown as { readsSkillUris?: boolean }).readsSkillUris).toBe(
      true,
    );
  });

  test("plain paths and http/file bash tokens never delegate", async () => {
    const s = setup();
    for (const [name, params] of [
      ["read", { path: "src/index.ts" }],
      ["write", { path: "src/index.ts", content: "text" }],
      ["edit", { path: "src/index.ts", appendContent: "text" }],
      ["grep", { path: "src", pattern: "text" }],
      ["bash", { command: "curl https://example.com", description: "Read the example HTTPS page" }],
      ["bash", { command: "curl http://example.com", description: "Read the example HTTP page" }],
      ["bash", { command: "cat file:///tmp/x", description: "Read the example file URI" }],
    ] as const)
      await executeTool(s.tools.get(name)!, params, s.extCtx);
    expect(s.invocations).toHaveLength(0);
    expect(s.calls).toHaveLength(7);
  });

  test("OMP router import failures warn once while upstream Pi stays silent", async () => {
    // A fresh module isolates the warn-once state from other host-init tests.
    const { loadOmpInternalUrlRouter } = await import("../omp-internal-urls.js?warning-test");
    const messages: string[] = [];
    const warning = spyOn(logger, "warn").mockImplementation((message) => {
      messages.push(message);
    });
    try {
      await loadOmpInternalUrlRouter({ registerTool: () => {} });
      expect(messages).toEqual([]);
      await loadOmpInternalUrlRouter({ arktype: {} });
      await loadOmpInternalUrlRouter({ arktype: {} });
      expect(messages).toHaveLength(1);
      expect(messages[0]).toContain("OMP internal URL delegation is unavailable");
      expect(messages[0]).toContain("internal-urls");
    } finally {
      warning.mockRestore();
    }
  });

  test("GitHub reads delegate but read-only OMP GitHub writes stay on AFT", async () => {
    const s = setup();
    await executeTool(s.tools.get("read")!, { path: "issue://5" }, s.extCtx);
    await executeTool(s.tools.get("write")!, { path: "issue://5", content: "comment" }, s.extCtx);
    await executeTool(
      s.tools.get("edit")!,
      { path: "pr://5/comments/1", edits: [{ oldString: "old", newString: "new" }] },
      s.extCtx,
    );
    expect(s.invocations).toEqual([{ path: "issue://5" }]);
    expect(s.calls.map((c) => c.params.name)).toEqual(["write", "edit", "edit"]);
  });

  test("schema audit accepts all native OMP edit modes, including hashline override", () => {
    for (const hashline of [false, true]) {
      const s = setup("omp", true, hashline);
      for (const params of [
        { path: "local://x", old_string: "old", new_string: "new", replace_all: true },
        {
          path: "local://x",
          edits: [{ op: "update", diff: "@@ -1 +1 @@\n-a\n+b", rename: "local://y" }],
        },
        { path: "local://x", edits: [] },
        { input: "native patch or hashline input" },
      ])
        expect(Value.Check(s.tools.get("edit")!.parameters as TSchema, params)).toBe(true);
    }
  });

  test("schema audit accepts native bash service and grep paging options", () => {
    const s = setup();
    expect(
      Value.Check(s.tools.get("bash")!.parameters as TSchema, {
        command: "pwd",
        description: "Start the example shell service",
        cwd: "local://",
        timeout: 0.5,
        pty: true,
        async: true,
        name: "service",
        ready: { log: "ready", port: 3000, host: "localhost", timeout: 0.5 },
      }),
    ).toBe(true);
    expect(
      Value.Check(s.tools.get("grep")!.parameters as TSchema, {
        pattern: "x",
        path: "local://",
        case: false,
        gitignore: false,
        skip: null,
      }),
    ).toBe(true);
  });

  test("hashline override delegates path-bearing native edits before AFT preflight", async () => {
    const s = setup("omp", true, true);
    const params = { path: "local://x", old_string: "old", new_string: "new" };
    await executeTool(s.tools.get("edit")!, params, s.extCtx);
    expect(s.invocations[0]).toBe(params);
    expect(s.calls).toHaveLength(0);
  });

  test("OMP local paths retain AFT schema validation", async () => {
    const s = setup();
    await expect(executeTool(s.tools.get("write")!, { path: "x" }, s.extCtx)).rejects.toThrow();
    await expect(
      executeTool(s.tools.get("edit")!, { path: "x", edits: [] }, s.extCtx),
    ).rejects.toThrow();
    await expect(
      executeTool(
        s.tools.get("bash")!,
        { command: "true", description: "Check shell timeout argument validation", timeout: 0.5 },
        s.extCtx,
      ),
    ).rejects.toThrow();
    await expect(executeTool(s.tools.get("bash")!, { command: "true" }, s.extCtx)).rejects.toThrow(
      /invalid AFT filesystem arguments: description/,
    );
    expect(s.calls).toHaveLength(0);
  });

  test("write filesystem errors name missing content and the AFT form", async () => {
    const s = setup();
    await expect(executeTool(s.tools.get("write")!, { path: "x" }, s.extCtx)).rejects.toThrow(
      /content.*required.*proc:\/\//,
    );
    expect(s.calls).toHaveLength(0);
  });

  test("read filesystem errors name paging arguments and the AFT form", async () => {
    const s = setup();
    await expect(
      executeTool(s.tools.get("read")!, { path: "x", offset: 0 }, s.extCtx),
    ).rejects.toThrow(/offset.*positive integers/);
    expect(s.calls).toHaveLength(0);
  });

  test("edit filesystem errors name native arguments and the AFT form", async () => {
    const s = setup();
    for (const [params, field] of [
      [{ path: "x", old_string: "old", new_string: "new" }, "old_string"],
      [{ path: "x", input: "native input" }, "input"],
      [{ path: "x", edits: [{ op: "update", diff: "patch", rename: "y" }] }, "edits[0].op"],
    ] as const) {
      let error: unknown;
      try {
        await executeTool(s.tools.get("edit")!, params, s.extCtx);
      } catch (caught) {
        error = caught;
      }
      expect(error).toBeInstanceOf(Error);
      expect((error as Error).message).toContain(field);
      expect((error as Error).message).toContain("oldString/newString");
      expect((error as Error).message).toContain("only for OMP internal URLs");
      const prepare = (
        s.tools.get("edit")! as unknown as { prepareArguments: (args: unknown) => unknown }
      ).prepareArguments;
      expect(() => prepare(params)).toThrow("only for OMP internal URLs");
    }
    const hashline = setup("omp", true, true);
    await expect(
      executeTool(
        hashline.tools.get("edit")!,
        { path: "x", old_string: "old", new_string: "new" },
        hashline.extCtx,
      ),
    ).rejects.toThrow(/old_string.*patch string/);
    expect(hashline.calls).toHaveLength(0);
    expect(s.calls).toHaveLength(0);
  });

  test("edit filesystem route drops inert native defaults from an AFT batch", async () => {
    await withFilesystemTool("edit", async (s) => {
      const tool = s.tools.get("edit")!;
      const { prepareArguments } = tool as unknown as {
        prepareArguments(args: unknown): Record<string, unknown>;
      };
      const control = {
        path: "src/example.ts",
        edits: [{ oldString: "before", newString: "after" }],
      };
      const entry = { ...control.edits[0], op: "update", rename: "", diff: "" };
      const defaults = { input: "", old_string: "", new_string: "", replace_all: false };
      const padded = { ...control, edits: [entry], ...defaults };
      expect(Value.Check(tool.parameters as TSchema, padded)).toBe(true);
      const prepared = prepareArguments(padded);
      expect(prepared).toEqual(control);
      await executeTool(tool, prepared, s.extCtx);
      await executeTool(tool, padded, s.extCtx);
      expect(s.calls.map(({ params }) => params.arguments)).toEqual([control, control]);

      const native = { ...padded, path: "local://fixture" };
      expect(prepareArguments(native)).toBe(native);
      await executeTool(tool, native, s.extCtx);
      expect(s.invocations[0]).toBe(native);
      expect(s.calls).toHaveLength(2);

      const deletion = {
        ...control,
        edits: [
          { oldString: "before", newString: "" },
          { startLine: 2, endLine: 2, content: "" },
        ],
      };
      await executeTool(
        tool,
        { ...deletion, ...defaults, edits: [{ ...entry, newString: "" }, deletion.edits[1]] },
        s.extCtx,
      );
      expect(s.calls[2]?.params.arguments).toEqual(deletion);

      for (const active of [
        { ...padded, input: "native input" },
        { ...padded, old_string: "native match", new_string: "" },
        { ...padded, new_string: "replacement" },
        { ...padded, replace_all: true },
        { ...padded, edits: [{ ...entry, rename: "other.ts" }] },
        { ...padded, edits: [{ ...entry, diff: "patch" }] },
        { ...padded, edits: [{ ...entry, op: "create" }] },
        { ...padded, edits: [{ ...entry, op: "delete" }] },
        { ...padded, appendContent: "append", edits: '[{"oldString":"","op":"delete"}]' },
        { ...padded, edits: [{ ...entry, oldString: "" }] },
        { ...padded, edits: [{ ...entry, startLine: 1, endLine: 1, content: "" }] },
        {
          ...control,
          ...defaults,
          edits: [{ ...control.edits[0], startLine: 1, endLine: 2, content: "" }],
        },
        { ...padded, appendContent: "another AFT mode" },
        { path: control.path, ...defaults },
      ]) {
        const refusal = /edit: invalid AFT filesystem arguments: .*only for OMP internal URLs/;
        expect(() => prepareArguments(active)).toThrow(refusal);
        await expect(executeTool(tool, active, s.extCtx)).rejects.toThrow(refusal);
      }
      expect(s.calls).toHaveLength(3);
      expect(s.invocations).toHaveLength(1);
    });
  });

  test("grep filesystem errors name native arguments and the AFT form", async () => {
    await withFilesystemTool("grep", async (s) => {
      const tool = s.tools.get("grep")!;
      const { prepareArguments } = tool as unknown as {
        prepareArguments(args: unknown): Record<string, unknown>;
      };
      for (const [field, value] of [
        ["gitignore", false],
        ["skip", 2],
        ["paths", ["src"]],
        ["path", []],
        ["path", ["src", ""]],
        ["path", ["src", 42]],
        ["case", "false"],
      ] as const) {
        const params = { path: "src", pattern: "x", case: false, skip: null, [field]: value };
        const refusal = `grep: invalid AFT filesystem arguments: ${field}.`;
        expect(() => prepareArguments(params)).toThrow(refusal);
        await expect(executeTool(tool, params, s.extCtx)).rejects.toThrow(refusal);
      }
      expect(s.calls).toHaveLength(0);
      expect(s.invocations).toHaveLength(0);
    });
  });

  test("bash filesystem errors name native arguments and the AFT form", async () => {
    const s = setup();
    await expect(
      executeTool(
        s.tools.get("bash")!,
        {
          command: "pwd",
          description: "Print the current working directory",
          cwd: "src",
          async: true,
          name: "service",
          ready: { port: 3000 },
        },
        s.extCtx,
      ),
    ).rejects.toThrow(/cwd.*async.*name.*ready.*workdir/);
    await expect(
      executeTool(
        s.tools.get("bash")!,
        { command: "pwd", description: "Print the current working directory", timeout: "0.5" },
        s.extCtx,
      ),
    ).rejects.toThrow(/timeout.*integer milliseconds/);
    await executeTool(
      s.tools.get("bash")!,
      { command: "pwd", description: "Print the current working directory", timeout: "500" },
      s.extCtx,
    );
    expect(s.calls).toHaveLength(1);
  });

  test("grep filesystem route accepts explicit paths and forwards the case flag", async () => {
    await withFilesystemTool("grep", async (s) => {
      const tool = s.tools.get("grep")!;
      const { prepareArguments } = tool as unknown as {
        prepareArguments(args: unknown): Record<string, unknown>;
      };
      const padded = {
        pattern: "example",
        path: ["src", "tests"],
        case: false,
        gitignore: true,
        paths: [],
        skip: null,
      };
      expect(Value.Check(tool.parameters as TSchema, padded)).toBe(true);
      await executeTool(tool, prepareArguments(padded), s.extCtx);
      await executeTool(tool, padded, s.extCtx);
      const expected = {
        pattern: "example",
        path: [join(s.root, "src"), join(s.root, "tests")],
        case_sensitive: false,
      };
      expect(s.calls.map(({ params }) => params.arguments)).toEqual([expected, expected]);

      const native = { ...padded, path: ["src", "local://fixture"] };
      expect(prepareArguments(native)).toBe(native);
      await executeTool(tool, native, s.extCtx);
      expect(s.invocations[0]).toBe(native);
      expect(s.calls).toHaveLength(2);

      mkdirSync(join(s.root, "src with spaces"));
      await executeTool(
        tool,
        { ...padded, path: ["src with spaces", "missing tests"], case: true, skip: 0 },
        s.extCtx,
      );
      expect(s.calls[2]?.params.arguments).toEqual({
        pattern: "example",
        path: [join(s.root, "src with spaces"), "missing tests"],
        case_sensitive: true,
      });

      const { approval } = tool as unknown as { approval(args: unknown): string };
      expect(approval(prepareArguments(padded))).toBe("read");
      const outside = { ...padded, path: ["src", "../outside with spaces", "tests"] };
      expect(approval(prepareArguments(outside))).toBe("exec");
      await expect(executeTool(tool, outside, s.extCtx)).rejects.toThrow(
        /outside with spaces.*outside the project root/,
      );
      expect(s.calls.slice(3).map(({ command, params }) => [command, params.path])).toEqual([
        ["bash_artifact_owned", join(s.root, "../outside with spaces")],
      ]);
    });
  });

  test("URL arguments without invokeTool render as normal AFT output", async () => {
    const s = setup();
    const params = { path: "agent://worker" };
    const tool = s.tools.get("read")!;
    const result = await executeTool(tool, params, makeExtContext());
    const rendered = renderToString(
      tool.renderResult!(
        result,
        { expanded: true },
        mockTheme,
        makeContext(params),
      ) as import("@earendil-works/pi-tui").Component,
    );
    expect(rendered).toBe("AFT");
    expect(s.invocations).toHaveLength(0);
  });

  test("delegated result attribution survives serialization without mutating native details", async () => {
    const s = setup();
    const tool = s.tools.get("read")!;
    const result = (await executeTool(tool, { path: "agent://worker" }, s.extCtx)) as {
      details: Record<string, unknown>;
    };
    expect(result.details.delegatedTo).toBe("omp");
    expect(s.nativeResult.details).not.toHaveProperty("delegatedTo");
    expect(result.details).not.toBe(s.nativeResult.details);
    const serialized = JSON.parse(JSON.stringify(result));
    const rendered = renderToString(
      tool.renderResult!(
        serialized,
        { expanded: true },
        mockTheme,
        makeContext({}),
      ) as import("@earendil-works/pi-tui").Component,
    );
    expect(rendered).toContain("OMP completed");
    expect(rendered).toContain("native status");
  });

  test("delegated progress copies carry renderer attribution without changing host updates", async () => {
    const s = setup();
    const tool = s.tools.get("read")!;
    const progress = {
      content: [{ type: "text", text: "native progress" }],
      details: { status: "running" },
    };
    let update: any;
    const context = Object.assign(makeExtContext(), {
      invokeTool: async (_params: unknown, options: { onUpdate?: (result: unknown) => void }) => {
        options.onUpdate?.(progress);
        return s.nativeResult;
      },
    });
    await tool.execute(
      "id",
      { path: "agent://worker" },
      undefined,
      (value) => {
        update = value;
      },
      context,
    );
    expect(update.details.delegatedTo).toBe("omp");
    expect(progress.details).not.toHaveProperty("delegatedTo");
    const rendered = renderToString(
      tool.renderResult!(
        JSON.parse(JSON.stringify(update)),
        { isPartial: true, expanded: true },
        mockTheme,
        makeContext({}),
      ) as import("@earendil-works/pi-tui").Component,
    );
    expect(rendered).toContain("agent://worker");
    expect(rendered).toContain("OMP running");
    expect(rendered).toContain("native progress");
  });

  test("plain Pi and hosts missing invokeTool or router retain AFT behavior", async () => {
    for (const mode of ["pi", "no-invoke", "no-router"]) {
      const s = setup(mode === "pi" ? "pi" : "omp", mode !== "no-router");
      await executeTool(
        s.tools.get("read")!,
        { path: "agent://worker" },
        mode === "no-invoke" ? makeExtContext() : s.extCtx,
      );
      expect(s.invocations).toHaveLength(0);
      expect(s.calls).toHaveLength(1);
      if (mode !== "no-invoke") {
        expect(
          Value.Check(s.tools.get("write")!.parameters as TSchema, { path: "proc://x/kill" }),
        ).toBe(false);
        expect(
          (s.tools.get("bash") as unknown as { readsSkillUris?: boolean }).readsSkillUris,
        ).toBeUndefined();
      }
    }
  });

  test("upstream Pi retains all five parameter schemas and local call results", async () => {
    const pi = setup("pi");
    const withoutRouter = setup("omp", false);
    for (const name of ["read", "write", "edit", "grep", "bash"]) {
      expect(pi.tools.get(name)!.parameters).toEqual(withoutRouter.tools.get(name)!.parameters);
    }
    const params = { path: "x", content: "text" };
    const plainContext = makeExtContext();
    expect(await executeTool(pi.tools.get("write")!, params, plainContext)).toEqual(
      await executeTool(withoutRouter.tools.get("write")!, params, plainContext),
    );
    expect(pi.calls).toEqual(withoutRouter.calls);
    expect(pi.invocations).toHaveLength(0);
  });

  test("delegated renderers show path, status and native text without assuming AFT details", async () => {
    const s = setup();
    for (const name of ["read", "write", "edit", "grep", "bash"]) {
      const params =
        name === "bash"
          ? { command: 'cat "skill://sample"', cwd: "skill://sample" }
          : { path: "proc://x/kill", content: "value", pattern: "x" };
      const tool = s.tools.get(name)!;
      const result = await executeTool(tool, params, s.extCtx);
      const component = tool.renderResult!(
        result,
        { expanded: true },
        mockTheme,
        makeContext(params),
      ) as import("@earendil-works/pi-tui").Component;
      const rendered = renderToString(component);
      expect(rendered).toContain(name === "bash" ? "skill://sample" : "proc://x/kill");
      expect(rendered).toContain("OMP");
      expect(rendered).toContain("native status");
      const nativeError = { content: [], details: null, isError: true };
      const errorResult = await executeTool(
        tool,
        params,
        Object.assign(makeExtContext(), { invokeTool: async () => nativeError }),
      );
      expect(nativeError.details).toBeNull();
      const error = tool.renderResult!(
        errorResult,
        {},
        mockTheme,
        makeContext(params, { isError: true }),
      ) as import("@earendil-works/pi-tui").Component;
      expect(renderToString(error)).toContain("error");
    }
  });
});
