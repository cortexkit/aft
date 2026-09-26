/// <reference path="../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, mock, spyOn, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  extractUserMessageText,
  shouldDetachBashWaitOnUserMessage,
  signalBashWaitDetachForProject,
  stripUserMessageDetachKeyword,
} from "../bash-wait-detach.js";
import * as logger from "../logger.js";

let projectRoot: string;

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-test-repo-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

describe("bash wait detach helper", () => {
  test("default config detaches on a plain user message", () => {
    expect(shouldDetachBashWaitOnUserMessage({}, "please continue")).toBe(true);
  });

  test("opt-out keeps a plain message blocking but &detach overrides it", () => {
    const config = { bash: { detach_on_user_message: false } };
    const plain = "please continue";
    const forced = "please &detach continue";

    expect(shouldDetachBashWaitOnUserMessage(config, plain)).toBe(false);
    expect(shouldDetachBashWaitOnUserMessage(config, forced)).toBe(true);
    expect(forced).toBe("please &detach continue");
  });

  test("strips every token while preserving the rest of a message", () => {
    const output = {
      parts: [{ type: "text", text: "before &detach middle &detach after" }],
    };

    expect(stripUserMessageDetachKeyword(output)).toBe("before middle after");
    expect(output.parts[0].text).toBe("before middle after");
  });

  test("recognizes standalone tokens at message boundaries", () => {
    const config = { bash: { detach_on_user_message: false } };
    const first = { parts: [{ type: "text", text: "&detach, continue" }] };
    const last = { parts: [{ type: "text", text: "continue &detach" }] };

    expect(shouldDetachBashWaitOnUserMessage(config, first.parts[0].text)).toBe(true);
    expect(shouldDetachBashWaitOnUserMessage(config, last.parts[0].text)).toBe(true);
    expect(stripUserMessageDetachKeyword(first)).toBe(", continue");
    expect(stripUserMessageDetachKeyword(last)).toBe("continue ");
  });

  test("does not detach or strip when the keyword is part of an identifier", () => {
    const config = { bash: { detach_on_user_message: false } };
    const messages = [
      "Please document &detachment behavior",
      "keep before&detach unchanged",
      "keep &detach_mode unchanged",
      "keep &detaché unchanged",
    ];

    for (const message of messages) {
      const output = { parts: [{ type: "text", text: message }] };
      expect(shouldDetachBashWaitOnUserMessage(config, message)).toBe(false);
      expect(stripUserMessageDetachKeyword(output)).toBe(message);
      expect(output.parts[0].text).toBe(message);
    }
  });

  test("substitutes an honest message when the token is the only user text", () => {
    const output = { parts: [{ type: "text", text: "  &detach  " }] };

    expect(stripUserMessageDetachKeyword(output)).toBe("(requested background detach)");
    expect(output.parts[0].text).toBe("(requested background detach)");
  });

  test("extracts only non-synthetic, non-ignored text parts", () => {
    const message = extractUserMessageText({
      parts: [
        { type: "text", text: "plain" },
        { type: "text", text: " synthetic", synthetic: true },
        { type: "text", text: " ignored", ignored: true },
      ],
    });

    expect(message).toBe("plain");
  });

  test("user-message detach sends bash_wait_detach on the active bridge", async () => {
    const calls: Array<[string, Record<string, unknown>, Record<string, unknown>]> = [];
    const bridge = {
      send: async (
        command: string,
        params: Record<string, unknown>,
        options: Record<string, unknown>,
      ) => {
        calls.push([command, params, options]);
        return { success: true, detached: true };
      },
    };
    const pool = {
      getActiveBridgeForRoot: (root: string) => {
        expect(root).toBe(projectRoot);
        return bridge;
      },
      activeBridges: () => [bridge],
    };

    await signalBashWaitDetachForProject(
      pool as Parameters<typeof signalBashWaitDetachForProject>[0],
      projectRoot,
      "session-1",
    );

    expect(calls).toHaveLength(1);
    expect(calls[0][0]).toBe("bash_wait_detach");
    expect(calls[0][1]).toEqual({ session_id: "session-1" });
    expect(calls[0][2]).toMatchObject({
      keepBridgeOnTimeout: true,
      transportTimeoutMs: 30_000,
    });
  });

  test("user-message detach is skipped without a session or any live bridge", async () => {
    const send = mock(async () => ({ success: true }));
    const pool = {
      getActiveBridgeForRoot: () => null,
      activeBridges: () => [],
    };

    await signalBashWaitDetachForProject(
      pool as Parameters<typeof signalBashWaitDetachForProject>[0],
      projectRoot,
      undefined,
    );
    await signalBashWaitDetachForProject(
      pool as Parameters<typeof signalBashWaitDetachForProject>[0],
      projectRoot,
      "session-2",
    );

    expect(send).not.toHaveBeenCalled();
  });

  test("no bridge yet is expected before the first tool call and is not a warning", async () => {
    const warn = spyOn(logger, "warn");
    const debug = spyOn(logger, "debug");
    try {
      await signalBashWaitDetachForProject(
        { getActiveBridgeForRoot: () => null, activeBridges: () => [] } as unknown as Parameters<
          typeof signalBashWaitDetachForProject
        >[0],
        projectRoot,
        "session-first-run",
      );
      expect(warn).not.toHaveBeenCalled();
      expect(debug).toHaveBeenCalledTimes(1);
      expect(String(debug.mock.calls[0]?.[0])).toContain("nothing to detach");
    } finally {
      warn.mockRestore();
      debug.mockRestore();
    }
  });

  test("root-key miss fans out to every live bridge instead of dropping", async () => {
    const sends: string[] = [];
    const bridgeFor = (label: string) => ({
      send: mock(async (command: string, params: Record<string, unknown>) => {
        sends.push(`${label}:${command}:${String(params.session_id)}`);
        return { success: true };
      }),
    });
    const bridgeA = bridgeFor("a");
    const bridgeB = bridgeFor("b");
    const pool = {
      // Exact root resolution misses (the silent-drop bug this guards):
      getActiveBridgeForRoot: () => null,
      activeBridges: () => [bridgeA, bridgeB],
    };

    await signalBashWaitDetachForProject(
      pool as unknown as Parameters<typeof signalBashWaitDetachForProject>[0],
      "/repo-that-does-not-match",
      "session-3",
    );

    expect(sends.sort()).toEqual(["a:bash_wait_detach:session-3", "b:bash_wait_detach:session-3"]);
  });
});
