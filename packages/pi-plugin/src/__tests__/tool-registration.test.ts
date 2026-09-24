/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import type { AftConfig } from "../config.js";
import {
  piHashlineDowngrade,
  piHashlineEffective,
  piPowerShellEnabledFromHost,
  registerPiToolSurface,
  resolvePiToolSurface,
} from "../tool-registration.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

function register(config: AftConfig) {
  const { api, tools } = makeMockApi();
  const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ok" }));
  const ctx = makePluginContext(bridge, { config });
  registerPiToolSurface(api, ctx, resolvePiToolSurface(config));
  return { tools, calls };
}

describe("Pi PowerShell registration", () => {
  test("uses the config fallback only when Pi's enabled-tool registry is unavailable", () => {
    expect(
      resolvePiToolSurface({ disabled_tools: [], bash: { powershell_tool: true } }).hoistPowershell,
    ).toBe(true);
    expect(resolvePiToolSurface({ disabled_tools: [], bash: {} }).hoistPowershell).toBe(false);

    const host = {
      getAllTools: () => [{ name: "powershell", sourceInfo: { source: "builtin" } }],
      getActiveTools: () => ["powershell"],
    } as any;
    expect(piPowerShellEnabledFromHost(host)).toBe(true);
    expect(resolvePiToolSurface({ disabled_tools: [], bash: {} }, host).hoistPowershell).toBe(true);

    host.getActiveTools = () => [];
    expect(
      resolvePiToolSurface({ disabled_tools: [], bash: { powershell_tool: true } }, host)
        .hoistPowershell,
    ).toBe(false);
  });

  test("registers PowerShell under its host name only", () => {
    const tools = register({ disabled_tools: [], bash: { powershell_tool: true } }).tools;
    expect(tools.has("powershell")).toBe(true);
    expect(tools.has("aft_powershell")).toBe(false);
  });
});

describe("Pi tool registration", () => {
  test("runtime gates and index switches never change the registered set", () => {
    const base = register({ disabled_tools: [] });
    const gated = register({
      disabled_tools: [],
      bash: { enabled: false, background: false },
      inspect: { enabled: false },
      backup: { enabled: false },
      indexes: { trigram: false, semantic: false, callgraph: false },
    });
    expect([...gated.tools.keys()].sort()).toEqual([...base.tools.keys()].sort());
  });

  test("host slots register under host names; no prefixed alternatives exist", async () => {
    const { tools, calls } = register({ disabled_tools: [] });
    for (const name of ["read", "write", "edit", "grep", "bash"]) {
      expect(tools.has(name)).toBe(true);
    }
    for (const name of ["aft_read", "aft_write", "aft_edit", "aft_grep", "aft_bash"]) {
      expect(tools.has(name)).toBe(false);
    }
    await executeTool(tools.get("edit")!, {
      path: "file.ts",
      edits: [{ oldString: "before", newString: "after" }],
    });
    expect(calls.at(-1)?.params.name).toBe("edit");
  });

  test("disabling bash leaves every companion registered", () => {
    const { tools } = register({ disabled_tools: ["bash"] });
    expect(tools.has("bash")).toBe(false);
    for (const name of ["bash_status", "bash_watch", "bash_write", "bash_kill"]) {
      expect(tools.has(name)).toBe(true);
    }
  });

  test("hashline needs the tagged read slot, not just the edit slot", () => {
    // With AFT's `read` registration removed, Pi keeps serving its own untagged
    // read while `edit` survives — nothing left to mint the tags a patch needs.
    const disabledRead: AftConfig = { edit_mode: "hashline", disabled_tools: ["read"] };
    const surface = resolvePiToolSurface(disabledRead);
    expect(surface.hoistEdit).toBe(true);
    expect(surface.hoistRead).toBe(false);
    expect(piHashlineEffective(disabledRead, surface)).toBe(false);
    expect(piHashlineDowngrade(disabledRead, surface)?.code).toBe("hashline_read_disabled");

    const enabled: AftConfig = { edit_mode: "hashline", disabled_tools: [] };
    const enabledSurface = resolvePiToolSurface(enabled);
    expect(piHashlineEffective(enabled, enabledSurface)).toBe(true);
    expect(piHashlineDowngrade(enabled, enabledSurface)).toBeNull();

    const disabledEdit: AftConfig = { edit_mode: "hashline", disabled_tools: ["edit"] };
    expect(piHashlineDowngrade(disabledEdit, resolvePiToolSurface(disabledEdit))?.code).toBe(
      "hashline_edit_disabled",
    );
    const both: AftConfig = { edit_mode: "hashline", disabled_tools: ["edit", "read"] };
    expect(piHashlineDowngrade(both, resolvePiToolSurface(both))?.code).toBe(
      "hashline_read_disabled",
    );
  });

  test("registration refuses a config without a resolved disabled list", () => {
    expect(() => resolvePiToolSurface({})).toThrow(
      "invalid_resolved_config:missing:disabled_tools",
    );
  });
});
