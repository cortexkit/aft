import { expect, test } from "bun:test";

test("durable plugin logging skips engine copies but keeps stderr and debug diagnostics", () => {
  const child = Bun.spawnSync({
    cmd: [
      process.execPath,
      "-e",
      `
      import { BinaryBridge } from "@cortexkit/aft-bridge";
      import { bridgeLogger } from "./src/logger.ts";
      const bridge = new BinaryBridge("/unused/aft", process.cwd(), { logger: bridgeLogger });
      for (const level of ["INFO", "DEBUG", "WARN", "ERROR"]) {
        bridge.onStderrData("2026-10-23T21:26:36.123Z [aft] " + level + " engine-only\\n");
      }
      bridge.onStderrData("unrecognized child diagnostic\\r\\n");
      bridgeLogger.debug("bridge debug", { sessionId: "ses_target" });
      console.log(bridge.formatStderrTail());
    `,
    ],
    env: { ...process.env, AFT_LOG_STDERR: "1" },
  });
  const stderr = child.stderr.toString();
  expect(child.exitCode).toBe(0);
  expect(stderr).not.toContain("engine-only");
  expect(stderr).toContain("INFO [aft-plugin] [aft] unrecognized child diagnostic\n");
  expect(stderr).toContain("DEBUG [aft-plugin] [ses_target] bridge debug\n");
  expect(child.stdout.toString()).toContain(
    "[aft] 2026-10-23T21:26:36.123Z [aft] WARN engine-only",
  );
});
