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
    await showAftStatusDialog(
      {} as ExtensionAPI,
      {
        cwd: projectRoot,
        sessionManager: { getSessionId: () => "new-session" },
        ui: {
          custom: async (factory: (...args: unknown[]) => unknown, options: unknown) => {
            expect(options).toEqual({
              overlay: true,
              overlayOptions: { anchor: "center", width: 84 },
            });
            let dismissed = false;
            const dialog = factory(
              { requestRender: () => renderCount++, terminal: { kittyProtocolActive: true } },
              {},
              {},
              () => {
                dismissed = true;
              },
            ) as { handleInput(data: string): void };
            // With Kitty active, a raw newline is a Shift+Enter binding, not Return.
            dialog.handleInput("\n");
            expect(dismissed).toBe(false);
            await waitUntil(() => calls.length === 1 && renderCount > 0);
            dialog.handleInput("\r");
            expect(dismissed).toBe(true);
          },
        },
      } as never,
      makePluginContext(bridge),
    );
    expect(calls[0].params).toEqual({ session_id: "new-session" });
  });

  test("OMP opens and dismisses status as a rows overlay without Pi positioning options", async () => {
    const { bridge } = makeMockBridge(() => ({
      success: true,
      version: "fixture",
      session: { id: "omp-session" },
    }));
    let repaints = 0;
    let dismissed = false;
    await showAftStatusDialog(
      { arktype: {} } as unknown as ExtensionAPI,
      {
        cwd: projectRoot,
        sessionManager: { getSessionId: () => "omp-session" },
        ui: {
          custom: async (factory: (...args: unknown[]) => unknown, options: unknown) => {
            expect(options).toEqual({ overlay: true });
            const identity = (text: string) => text;
            const theme = { fg: (_tone: string, text: string) => text, bold: identity };
            const dialog = factory({ requestRender: () => repaints++ }, theme, {}, () => {
              dismissed = true;
            }) as { render(width: number): string[]; handleInput(data: string): void };
            await waitUntil(() => repaints > 0);
            expect(dialog.render(84).join("\n")).toContain("AFT Status   vfixture");
            dialog.handleInput("\u001b[27u");
            expect(dismissed).toBe(true);
          },
        },
      } as never,
      makePluginContext(bridge),
    );
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
