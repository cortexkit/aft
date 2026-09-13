/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { resolveOmpPaths } from "../lib/omp-paths.js";

let root: string;
let releaseEnv: (() => void) | undefined;

beforeEach(async () => {
  root = mkdtempSync(join(tmpdir(), "aft-cli-omp-paths-"));
  releaseEnv = await acquireEnv({
    HOME: root,
    USERPROFILE: root,
    XDG_CONFIG_HOME: undefined,
    XDG_DATA_HOME: undefined,
    PI_CONFIG_DIR: undefined,
    PI_CODING_AGENT_DIR: undefined,
    PI_PACKAGE_DIR: undefined,
    PI_PROFILE: undefined,
    PI_CONFIG_FILES: undefined,
    OMP_PROFILE: undefined,
  });
});

afterEach(() => {
  releaseEnv?.();
  releaseEnv = undefined;
  rmSync(root, { recursive: true, force: true });
});

describe("OMP path compatibility", () => {
  test("resolves the default layout", () => {
    expect(resolveOmpPaths()).toEqual({
      configRoot: join(root, ".omp"),
      agentDir: join(root, ".omp", "agent"),
      dataRoot: join(root, ".omp"),
      dataAgentRoot: join(root, ".omp", "agent"),
      pluginsDir: join(root, ".omp", "plugins"),
      sessionsRoot: join(root, ".omp", "agent", "sessions"),
    });
  });

  test("honors PI_CONFIG_DIR", () => {
    process.env.PI_CONFIG_DIR = ".custom-omp";

    expect(resolveOmpPaths()).toMatchObject({
      configRoot: join(root, ".custom-omp"),
      agentDir: join(root, ".custom-omp", "agent"),
      pluginsDir: join(root, ".custom-omp", "plugins"),
    });
  });

  test("puts a validated OMP_PROFILE under the profile root", () => {
    process.env.OMP_PROFILE = "work.1";

    expect(resolveOmpPaths()).toMatchObject({
      configRoot: join(root, ".omp", "profiles", "work.1"),
      agentDir: join(root, ".omp", "profiles", "work.1", "agent"),
      pluginsDir: join(root, ".omp", "profiles", "work.1", "plugins"),
    });
  });

  test("uses PI_PROFILE when OMP_PROFILE is unset", () => {
    process.env.PI_PROFILE = "fallback";

    expect(resolveOmpPaths().configRoot).toBe(join(root, ".omp", "profiles", "fallback"));
  });

  test("treats default and invalid profile names as the unprofiled layout", () => {
    process.env.OMP_PROFILE = "default";
    expect(resolveOmpPaths().configRoot).toBe(join(root, ".omp"));

    process.env.OMP_PROFILE = "Uppercase Is Invalid";
    expect(resolveOmpPaths().configRoot).toBe(join(root, ".omp"));
  });

  test("honors PI_CODING_AGENT_DIR only without a named profile", () => {
    const customAgent = join(root, "custom-agent");
    process.env.PI_CODING_AGENT_DIR = customAgent;
    expect(resolveOmpPaths()).toMatchObject({
      agentDir: customAgent,
      sessionsRoot: join(customAgent, "sessions"),
      pluginsDir: join(root, ".omp", "plugins"),
    });

    process.env.OMP_PROFILE = "work";
    expect(resolveOmpPaths().agentDir).toBe(join(root, ".omp", "profiles", "work", "agent"));
  });

  test("keeps config-root data when the XDG OMP root is absent", () => {
    process.env.XDG_DATA_HOME = join(root, "xdg-data");

    expect(resolveOmpPaths()).toMatchObject({
      dataRoot: join(root, ".omp"),
      pluginsDir: join(root, ".omp", "plugins"),
      sessionsRoot: join(root, ".omp", "agent", "sessions"),
    });
  });

  test("uses an initialized XDG data root and flattens the agent prefix", () => {
    const xdgData = join(root, "xdg-data");
    mkdirSync(join(xdgData, "omp"), { recursive: true });
    process.env.XDG_DATA_HOME = xdgData;

    const paths = resolveOmpPaths();
    if (process.platform === "linux" || process.platform === "darwin") {
      expect(paths).toMatchObject({
        dataRoot: join(xdgData, "omp"),
        dataAgentRoot: join(xdgData, "omp"),
        pluginsDir: join(xdgData, "omp", "plugins"),
        sessionsRoot: join(xdgData, "omp", "sessions"),
      });
    } else {
      expect(paths.dataRoot).toBe(join(root, ".omp"));
    }
  });

  test("requires an initialized profile-specific XDG root", () => {
    const xdgData = join(root, "xdg-data");
    mkdirSync(join(xdgData, "omp"), { recursive: true });
    process.env.XDG_DATA_HOME = xdgData;
    process.env.OMP_PROFILE = "work";

    expect(resolveOmpPaths().dataRoot).toBe(join(root, ".omp", "profiles", "work"));

    mkdirSync(join(xdgData, "omp", "profiles", "work"), { recursive: true });
    const paths = resolveOmpPaths();
    if (process.platform === "linux" || process.platform === "darwin") {
      expect(paths.pluginsDir).toBe(join(xdgData, "omp", "profiles", "work", "plugins"));
      expect(paths.sessionsRoot).toBe(join(xdgData, "omp", "profiles", "work", "sessions"));
    }
  });
});
