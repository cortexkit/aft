/// <reference path="../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { showAftStatusDialog } from "../dialogs/status-dialog.js";
import { makeMockBridge, makePluginContext } from "./tool-test-utils.js";

let projectRoot: string;

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-test-repo-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

describe("aft-status dialog cache", () => {
  test("fetches fresh status when cached snapshot belongs to another session", async () => {
    const { bridge, calls } = makeMockBridge((_command, params) => ({
      success: true,
      version: "fresh",
      session: { id: params.session_id },
    }));
    bridge.cacheStatusSnapshot({ version: "stale", session: { id: "old-session" } });

    let renderCount = 0;
    let component: { dispose?: () => void } | undefined;
    try {
      await showAftStatusDialog(
        {} as ExtensionAPI,
        {
          cwd: projectRoot,
          sessionManager: { getSessionId: () => "new-session" },
          ui: {
            custom: async (factory: (...args: unknown[]) => unknown) => {
              component = factory(
                { requestRender: () => renderCount++ },
                {},
                {},
                () => undefined,
              ) as { dispose?: () => void };
            },
          },
        } as never,
        makePluginContext(bridge),
      );

      await waitUntil(() => calls.length === 1 && renderCount > 0);
      expect(calls[0].params).toEqual({ session_id: "new-session" });
    } finally {
      component?.dispose?.();
    }
  });

  test("does not overlap status requests while the initial fetch is pending", async () => {
    let resolveStatus!: (value: Record<string, unknown>) => void;
    const pendingStatus = new Promise<Record<string, unknown>>((resolve) => {
      resolveStatus = resolve;
    });
    const { bridge, calls } = makeMockBridge(() => pendingStatus);
    let component: { dispose?: () => void } | undefined;
    try {
      await showAftStatusDialog(
        {} as ExtensionAPI,
        {
          cwd: projectRoot,
          sessionManager: { getSessionId: () => "session" },
          ui: {
            custom: async (factory: (...args: unknown[]) => unknown) => {
              component = factory({ requestRender: () => undefined }, {}, {}, () => undefined) as {
                dispose?: () => void;
              };
            },
          },
        } as never,
        makePluginContext(bridge),
      );

      await waitUntil(() => calls.length === 1);
      await new Promise((resolve) => setTimeout(resolve, 1_600));
      expect(calls).toHaveLength(1);
    } finally {
      resolveStatus({ success: true, session: { id: "session" } });
      component?.dispose?.();
    }
  });
});

async function waitUntil(predicate: () => boolean, timeoutMs = 5_000): Promise<void> {
  const started = Date.now();
  while (!predicate()) {
    if (Date.now() - started > timeoutMs) throw new Error("timed out waiting for condition");
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
}
