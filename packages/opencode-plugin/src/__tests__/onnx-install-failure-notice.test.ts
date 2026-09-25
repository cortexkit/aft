/// <reference path="../bun-test.d.ts" />

/**
 * A failed ONNX Runtime install must reach the user, not only the plugin log.
 * Semantic search is on by default, so without this notice a clean machine
 * whose install failed shows a feature that silently never works.
 */
import { describe, expect, test } from "bun:test";
import {
  type BridgeBootstrapDependencies,
  defaultBridgeBootstrapDependencies,
  onnxInstallFailureMessage,
  prepareBridgeEnvironment,
} from "../bridge-bootstrap.js";
import type { AftConfig } from "../config.js";

function dependencies(
  overrides: Partial<BridgeBootstrapDependencies>,
): BridgeBootstrapDependencies {
  return {
    ...defaultBridgeBootstrapDependencies,
    resolveBinary: async () => "/never-spawned/aft",
    ensureStorageMigrated: async () => {},
    resolveStorageRoot: () => "/never-used/storage",
    buildConfigureParams: (_directory, state) => ({ ...state }),
    startLspAutoInstall: () => null,
    isOrtAutoDownloadSupported: () => true,
    ...overrides,
  };
}

async function bootAndAttach(deps: BridgeBootstrapDependencies): Promise<string[]> {
  const notices: string[] = [];
  const environment = await prepareBridgeEnvironment(
    {
      configRoot: "/project",
      lspDirectory: "/project",
      config: { indexes: { semantic: true }, disabled_tools: [] } as unknown as AftConfig,
      pluginVersion: "0.0.0-test",
      notify: (message) => notices.push(message),
    },
    deps,
  );
  const overrides: Record<string, unknown> = {};
  environment.attach({
    setConfigureOverride: (key: string, value: unknown) => {
      overrides[key] = value;
    },
  } as never);
  await environment.onnxRuntime;
  await new Promise((resolve) => setTimeout(resolve, 0));
  return notices;
}

describe("ONNX Runtime install failure notice", () => {
  test("a failed install tells the user semantic search is unavailable and why", async () => {
    const notices = await bootAndAttach(
      dependencies({
        ensureOnnxRuntime: async () => null,
        onnxInstallFailure: () => "ONNX Runtime download failed: HTTP 503",
      }),
    );

    expect(notices).toEqual([onnxInstallFailureMessage("ONNX Runtime download failed: HTTP 503")]);
    expect(notices[0]).toContain("Semantic search is unavailable");
    expect(notices[0]).toContain("HTTP 503");
  });

  test("a successful install sends no notice", async () => {
    const notices = await bootAndAttach(
      dependencies({
        ensureOnnxRuntime: async () => "/managed/onnxruntime/1.24.4",
        onnxInstallFailure: () => "stale reason that must not be shown",
      }),
    );

    expect(notices).toEqual([]);
  });
});
