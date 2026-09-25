/// <reference path="../bun-test.d.ts" />

/**
 * The configuration-facing pieces of the bootstrap both OpenCode entries
 * share: rejection, the per-project acceptance check, and the registration
 * warnings each entry reports the same way.
 */

import { describe, expect, test } from "bun:test";

import {
  type BridgeBootstrapDependencies,
  createProjectAcceptance,
  defaultSurfaceConfig,
  loadBootstrapConfig,
  reportHashlineDowngrade,
  resolveBootstrapConfig,
  unknownDisabledToolsReporter,
} from "../bridge-bootstrap.js";
import { type AftConfig, ConfigRejectedError } from "../config.js";

function dependencies(loadConfig: (directory: string) => AftConfig): BridgeBootstrapDependencies {
  return {
    loadConfig,
    migrateConfigLocations: () => ["migrated"],
  } as unknown as BridgeBootstrapDependencies;
}

const rejecting = (directory: string): AftConfig => {
  throw new ConfigRejectedError(["removed_config_key:aft_glob:use:glob"], directory);
};

describe("bootstrap configuration", () => {
  test("a rejected configuration yields the config error state, reports once, and skips migration", () => {
    const notices: string[] = [];
    const result = loadBootstrapConfig(
      "/p",
      (message) => notices.push(message),
      dependencies(rejecting),
    );
    expect(result.ok).toBe(false);
    if (result.ok) throw new Error("unreachable");
    expect(result.message).toContain("removed_config_key:aft_glob:use:glob");
    expect(result.message).toContain("npx @cortexkit/aft doctor --fix");
    expect(result.message).toContain("restart");
    // Too broken to compute its own surface: the default surface registers.
    expect(result.config).toEqual(defaultSurfaceConfig());
    expect(notices).toEqual([result.message]);
  });

  test("a config file that does not parse yields the config error state", () => {
    const notices: string[] = [];
    const result = loadBootstrapConfig("/p", (message) => notices.push(message), {
      ...dependencies(() => ({ disabled_tools: [] }) as AftConfig),
      configLoadErrors: () => [{ path: "/p/aft.jsonc", message: "Unexpected token }" }],
    });
    expect(result.ok).toBe(false);
    if (result.ok) throw new Error("unreachable");
    expect(result.message).toContain("/p/aft.jsonc failed to parse: Unexpected token }");
    expect(result.message).toContain("Fix the JSONC syntax");
    expect(result.config).toEqual(defaultSurfaceConfig());
    // Migration never ran: the only notice is the error itself.
    expect(notices).toEqual([result.message]);
  });

  test("a missing subc connection file yields the config error state with the loaded surface", async () => {
    const config = {
      disabled_tools: ["aft_move"],
      subc: { connection_file: "/nope" },
    } as AftConfig;
    const result = await resolveBootstrapConfig("/p", () => {}, {
      ...dependencies(() => config),
      subcConnectionFileError: async (file) => (file ? `missing ${file}` : null),
    });
    expect(result).toEqual({
      ok: false,
      message: expect.stringContaining("missing /nope."),
      config,
    });
  });

  test("the retired top-level enabled key no longer stops the boot", () => {
    const notices: string[] = [];
    const config = { enabled: false, disabled_tools: [] } as unknown as AftConfig;
    expect(
      loadBootstrapConfig(
        "/p",
        (message) => notices.push(message),
        dependencies(() => config),
      ),
    ).toEqual({ ok: true, config });
    expect(notices).toEqual(["migrated"]);
  });

  test("project acceptance refuses only rejected configurations and caches the answer", () => {
    const loads: string[] = [];
    const accepted = createProjectAcceptance("/boot", {
      loadConfig: (directory) => {
        loads.push(directory);
        if (directory === "/bad") return rejecting(directory);
        return { disabled_tools: [] } as AftConfig;
      },
    });
    expect(accepted("/boot")).toBe(true);
    expect(accepted("/ok")).toBe(true);
    expect(accepted("/bad")).toBe(false);
    expect(accepted("/bad")).toBe(false);
    expect(loads).toEqual(["/ok", "/bad"]);
  });

  test("unknown disabled names become one aggregated warning", () => {
    const notices: string[] = [];
    unknownDisabledToolsReporter((message) => notices.push(message))(["aft_future", "typo"]);
    expect(notices).toEqual([
      "unknown_disabled_tools: disabled_tools lists names AFT does not know: aft_future, typo",
    ]);
  });

  test("a hashline surface without the tagged read reports hashline_read_disabled", () => {
    const notices: string[] = [];
    const config = { edit_mode: "hashline", disabled_tools: ["read"] } as AftConfig;
    const downgrade = reportHashlineDowngrade(config, new Set(["edit"]), (message) =>
      notices.push(message),
    );
    expect(downgrade?.code).toBe("hashline_read_disabled");
    expect(notices).toHaveLength(1);

    const intact = { edit_mode: "hashline", disabled_tools: [] } as AftConfig;
    expect(reportHashlineDowngrade(intact, new Set(["edit", "read"]), () => {})).toBeNull();
  });
});
