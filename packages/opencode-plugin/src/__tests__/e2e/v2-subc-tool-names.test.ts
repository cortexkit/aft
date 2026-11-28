/**
 * OpenCode 2 on the subc daemon: AFT's tool names follow each caller.
 *
 * Under the subc daemon one AFT module serves every head bound to a project:
 * an OpenCode 1 head (which names AFT's tools `bash` and `apply_patch`) and an
 * OpenCode 2 session (which registers them as `shell` and `patch`) can be bound
 * to the same root at once. RouteBind carries no field for the names, so the
 * OpenCode 2 plugin sends them beside every call. This test starts an isolated
 * daemon (the aft-bridge subc rig: throwaway HOME, XDG roots, storage and
 * connection file), points a real OpenCode 2 host's AFT plugin at it, and binds
 * an OpenCode 1-style head to the same root through the same daemon. Their
 * calls interleave, and each reply names the caller's own command tool.
 *
 * Needs AFT_V2_HOST_BINARY, a built `dist/`, an `aft` binary built from this
 * checkout, and a subc-core binary (SUBC_CORE_BIN, or the locations the rig
 * searches); without a subc-core binary the test is skipped with the reason.
 */
import { expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { type AftTransportPool, createAftTransportPool } from "@cortexkit/aft-bridge";

import {
  prepareSubcLane,
  startSubcRig,
} from "../../../../aft-bridge/src/__tests__/e2e/subc-rig.js";
import { aftBinary, bootV2Host, toolResult } from "./v2-host.js";

const hostBinary = process.env.AFT_V2_HOST_BINARY;
const REWRITE = "cat probe.txt";

test.skipIf(!hostBinary)(
  "real V2 host on the subc daemon names its own tools while an OpenCode 1 head shares the root",
  async () => {
    expect(existsSync(aftBinary)).toBe(true);
    const prepared = await prepareSubcLane();
    if (!prepared.subcCorePath || !prepared.aftBinaryPath) {
      console.log(`[v2-subc-tool-names] skipped: ${prepared.skipReason}`);
      return;
    }
    const rig = await startSubcRig(prepared);
    let head: AftTransportPool | undefined;
    const host = await bootV2Host(hostBinary as string, {
      label: "subc-tool-names",
      removeBuiltins: true,
      files: { "probe.txt": "rewrite-probe-line\n" },
      // Presence of a connection file puts the plugin in subc mode.
      userAftConfig: { subc: { connection_file: rig.connectionFile } },
      script: {
        "run the rewrite probe": {
          name: "shell",
          arguments: { command: REWRITE, description: "Read the rewrite probe file" },
        },
      },
    }).catch(async (error) => {
      await rig.cleanup();
      throw error;
    });
    try {
      console.log(`[v2-subc-tool-names] OpenCode ${host.version}; daemon ${rig.connectionFile}`);
      // An OpenCode 1-style head on the same daemon and root, sending no names.
      head = await createAftTransportPool({
        harness: "opencode",
        binaryPath: prepared.aftBinaryPath,
        poolOptions: { timeoutMs: 30_000 },
        configOverrides: {},
        subcConnectionFile: rig.connectionFile,
        subcConsumerIdentity: null,
      });
      const headBridge = head.getBridge(host.project);
      const headCat = async () => {
        const reply = await headBridge.send("bash", {
          session_id: "opencode-1-head",
          command: REWRITE,
          description: "Read the rewrite probe file",
          workdir: host.project,
          compressed: false,
        });
        return String(reply.output ?? reply.text ?? JSON.stringify(reply));
      };

      const session = await host.newSession("Subc tool names");
      for (let round = 0; round < 2; round += 1) {
        await host.turn(session.id, "run the rewrite probe");
        const v2 = host.requests
          .flatMap((request) => request.messages)
          .filter((message) => message.role === "tool")
          .map((message) => (typeof message.content === "string" ? message.content : ""))
          .at(-1);
        const v1 = await headCat();
        console.log(
          `[v2-subc-tool-names] round ${round}: v2=${JSON.stringify(v2)} v1=${JSON.stringify(v1)}`,
        );
        expect(v2 ?? toolResult(host.requests, REWRITE)).toContain(
          "Prefer `read` tool over shell.",
        );
        expect(v1).toContain("rewrite-probe-line");
        expect(v1).toContain("Prefer `read` tool over bash.");
      }

      // Both callers really went through the daemon's AFT module: it logged a
      // route bind for the OpenCode 2 session and for the head.
      const moduleLog = await readFile(join(rig.logDir, "aft.stderr.log"), "utf8");
      expect(moduleLog).toContain(`session=${session.id}`);
      expect(moduleLog).toContain("session=opencode-1-head");
    } catch (error) {
      throw new Error(`${String(error)}\nV2 host output:\n${host.logs().slice(-20_000)}`, {
        cause: error,
      });
    } finally {
      await head?.shutdown().catch(() => undefined);
      await host.stop();
      await rig.cleanup();
    }
  },
  240_000,
);
