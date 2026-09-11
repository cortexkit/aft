/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { SubcError } from "@cortexkit/subc-client";
import {
  BASH_HOST_FALLBACK_BANNER,
  bashHostFallbackAskPattern,
  hostFallbackPathWithShims,
  runBashHostFallback,
} from "../bash-host-fallback.js";
import { classifyBashHostFallbackError } from "../error-contract.js";
import { resolveCortexKitStorageRoot } from "../storage-paths.js";

interface FakeBashTransport {
  send(command: "bash"): Promise<Record<string, unknown>>;
}

async function callWithHostFallback(options: {
  transport: FakeBashTransport;
  hostFallback: boolean;
  command: string;
  projectRoot: string;
  ask: (pattern: string) => Promise<boolean>;
}): Promise<Record<string, unknown>> {
  try {
    return await options.transport.send("bash");
  } catch (error) {
    const cause = classifyBashHostFallbackError(error);
    if (!options.hostFallback || cause === undefined) throw error;
    const approved = await options.ask(
      bashHostFallbackAskPattern(options.command, options.projectRoot, cause),
    );
    if (!approved) throw new Error("Permission denied: AFT host fallback execution was denied.");
    return await runBashHostFallback({
      command: options.command,
      projectRoot: options.projectRoot,
      timeoutMs: 5_000,
    });
  }
}

function moduleUnavailableTransport(): FakeBashTransport {
  return {
    send: async () => {
      throw new SubcError(
        "module_id 'aft' is supervised but not available (state=stopped, enabled=true, live=false) The AFT daemon module did not return within the 15s reload window.",
        "module_warming",
      );
    },
  };
}

function markerCommand(marker: string): string {
  const script = `(async () => { const fs = await import("node:fs"); fs.writeFileSync(${JSON.stringify(marker)}, "ran"); process.stdout.write("host-fallback-ran"); })()`;
  const encoded = Buffer.from(script).toString("base64");
  return `${JSON.stringify(process.execPath)} -e "eval(Buffer.from('${encoded}', 'base64').toString())"`;
}

describe("bash host fallback", () => {
  test.each([
    "module down",
    "bind timed out",
    "route closed before dispatch",
  ] as const)("permission pattern names the %s cause and carries the exact command and project root", (cause) => {
    const command = "printf 'exact  value'\nprintf done";
    const cwd = "/tmp/project with spaces";

    expect(bashHostFallbackAskPattern(command, cwd, cause)).toBe(
      `AFT UNAVAILABLE (${cause}) - host fallback execution:\n\nExact command:\n${command}\n\nWorking directory:\n${cwd}`,
    );
  });

  test("module-unavailable fallback executes the approved command on the host", async () => {
    const projectRoot = await mkdtemp(join(tmpdir(), "aft-host-fallback-approved-"));
    const marker = join(projectRoot, "ran.txt");
    const prompts: string[] = [];
    try {
      const result = await callWithHostFallback({
        transport: moduleUnavailableTransport(),
        hostFallback: true,
        command: markerCommand(marker),
        projectRoot,
        ask: async (pattern) => {
          prompts.push(pattern);
          return true;
        },
      });

      expect(existsSync(marker), String(result.output)).toBe(true);
      expect(result.output).toContain("host-fallback-ran");
      expect(prompts).toHaveLength(1);
      expect(prompts[0]).toStartWith("AFT UNAVAILABLE (module down)");
    } finally {
      await rm(projectRoot, { recursive: true, force: true });
    }
  });

  test("module-unavailable fallback does not execute a declined command", async () => {
    const projectRoot = await mkdtemp(join(tmpdir(), "aft-host-fallback-declined-"));
    const marker = join(projectRoot, "ran.txt");
    try {
      await expect(
        callWithHostFallback({
          transport: moduleUnavailableTransport(),
          hostFallback: true,
          command: markerCommand(marker),
          projectRoot,
          ask: async () => false,
        }),
      ).rejects.toThrow("host fallback execution was denied");
      expect(existsSync(marker)).toBe(false);
    } finally {
      await rm(projectRoot, { recursive: true, force: true });
    }
  });

  test("captures real stdout and stderr with banner and exit code", async () => {
    const result = await runBashHostFallback({
      command:
        process.platform === "win32"
          ? `${JSON.stringify(process.execPath)} -e "process.stdout.write('stdout'); process.stderr.write('stderr'); process.exit(3)"`
          : "printf stdout; printf stderr >&2; exit 3",
      projectRoot: process.cwd(),
      timeoutMs: 5_000,
    });

    expect(result.output).toStartWith(`${BASH_HOST_FALLBACK_BANNER}\n`);
    expect(result.output).toContain("stdout");
    expect(result.output).toContain("stderr");
    expect(result.output).toEndWith("[exit code: 3]");
    expect(result.exit_code).toBe(3);
  });

  test("keeps only the 100 KB output tail", async () => {
    const result = await runBashHostFallback({
      command: `${JSON.stringify(process.execPath)} -e "process.stdout.write('x'.repeat(120 * 1024)); process.stdout.write('TAIL')"`,
      projectRoot: process.cwd(),
      timeoutMs: 5_000,
    });

    expect(result.truncated).toBe(true);
    expect(result.output).toContain("TAIL");
    expect(Buffer.byteLength(result.output)).toBeLessThanOrEqual(100 * 1024 + 200);
  });

  test("hard timeout kills the command and reports exit code 124", async () => {
    const result = await runBashHostFallback({
      command: `${JSON.stringify(process.execPath)} -e "setInterval(() => {}, 1000)"`,
      projectRoot: process.cwd(),
      timeoutMs: 20,
    });

    expect(result.exit_code).toBe(124);
    expect(result.output).toEndWith("[exit code: 124]");
  });

  test("an abort kills the inline child and rejects promptly", async () => {
    const controller = new AbortController();
    const started = Date.now();
    const running = runBashHostFallback({
      command: `${JSON.stringify(process.execPath)} -e "setInterval(() => {}, 1000)"`,
      projectRoot: process.cwd(),
      signal: controller.signal,
    });

    setTimeout(() => controller.abort(), 25);

    await expect(running).rejects.toMatchObject({ name: "AbortError" });
    expect(Date.now() - started).toBeLessThan(2_000);
  });
});

describe("hostFallbackPathWithShims", () => {
  const shimsRoot = resolveCortexKitStorageRoot();
  const shimsDir = join(shimsRoot, "shims");
  const hasShim = existsSync(join(shimsDir, "gh"));

  test("prepends the shims dir exactly once when the gh shim exists", () => {
    if (!hasShim) return; // machine without a provisioned shim: covered by the absent case below
    const path = hostFallbackPathWithShims({ PATH: `/usr/bin:${shimsDir}:/bin` });
    expect(path?.split(delimiter)[0]).toBe(shimsDir);
    expect(path?.split(delimiter).filter((entry) => entry === shimsDir)).toHaveLength(1);
  });

  test("leaves PATH untouched when no shim is provisioned", () => {
    // Point storage at an empty location via env override.
    const env = { PATH: "/usr/bin:/bin", AFT_STORAGE_DIR: "/nonexistent-aft-storage-root" };
    expect(hostFallbackPathWithShims(env)).toBe("/usr/bin:/bin");
  });

  test("uses the inherited Windows Path value when variants collide", () => {
    const env = {
      Path: "C:\\Windows\\System32;C:\\Git\\cmd",
      PATH: "C:\\stale",
      AFT_STORAGE_DIR: "C:\\nonexistent-aft-storage-root",
    };

    expect(hostFallbackPathWithShims(env, "win32")).toBe("C:\\Windows\\System32;C:\\Git\\cmd");
  });
});
