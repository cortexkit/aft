import { describe, expect, spyOn, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import {
  AftConfigSchema,
  getConfigLoadSources,
  getConfigLoadTexts,
  loadAftConfig,
  resolveProjectOverridesForConfigure,
} from "../config.js";
import { startPiLiveConfigReload } from "../config-live-reload.js";
import * as logger from "../logger.js";

const limits = {
  max_rounds: 3,
  max_nodes: 12,
  max_frontier: 40,
  decision_timeout_ms: 3000,
  deadline_ms: 8000,
  max_output_bytes: 12288,
};
const decision = {
  backend: "decisions",
  endpoint: "https://example.test/decision",
  model: "test-model",
  api_key_env: "TEST_GATHER_KEY",
  threshold: 0.7,
};

async function fixture(
  run: (f: {
    root: string;
    user: (value: unknown) => void;
    project: (value: unknown) => void;
  }) => void,
) {
  const dir = mkdtempSync(join(tmpdir(), "aft-pi-gather-config-"));
  const root = join(dir, "project");
  mkdirSync(join(dir, "cortexkit"), { recursive: true });
  mkdirSync(join(root, ".cortexkit"), { recursive: true });
  const release = await acquireEnv({
    HOME: join(dir, "home"),
    XDG_CONFIG_HOME: dir,
    OPENCODE_CONFIG_DIR: undefined,
  });
  try {
    run({
      root,
      user: (value) => writeFileSync(join(dir, "cortexkit/aft.jsonc"), JSON.stringify(value)),
      project: (value) => writeFileSync(join(root, ".cortexkit/aft.jsonc"), JSON.stringify(value)),
    });
  } finally {
    release();
    rmSync(dir, { recursive: true, force: true });
  }
}

describe.serial("gather config", () => {
  test("decisions threshold accepts only probabilities strictly between zero and one", () => {
    expect(AftConfigSchema.safeParse({ search: { gather: { decision } } }).success).toBe(true);
    for (const threshold of [0, 1, -0.1, 1.1, "0.5", Number.NaN, Number.POSITIVE_INFINITY]) {
      expect(
        AftConfigSchema.safeParse({ search: { gather: { decision: { threshold } } } }).success,
      ).toBe(false);
    }
  });
  test("public loader enforces enablement in base and active harness tiers", async () => {
    await fixture((f) => {
      const cases = [
        { user: {}, project: {}, enabled: false },
        { user: { enabled: true }, project: {}, enabled: true },
        { user: { enabled: true }, project: { enabled: false }, enabled: false },
        { user: {}, project: { enabled: true }, enabled: false },
        { user: { enabled: false }, project: { enabled: true }, enabled: false },
      ];
      for (const harness of [false, true]) {
        for (const c of cases) {
          const doc = (gather: unknown) =>
            harness ? { harnesses: { pi: { search: { gather } } } } : { search: { gather } };
          f.user(doc(c.user));
          f.project(doc(c.project));
          expect(loadAftConfig(f.root).search?.gather?.enabled === true).toBe(c.enabled);
        }
      }
    });
  });
  test("every project decision and limit is dropped with its full key path", async () => {
    await fixture((f) => {
      const warn = spyOn(logger, "warn");
      try {
        for (const harness of [false, true]) {
          warn.mockClear();
          f.user({ search: { gather: { enabled: true, max_nodes: 7 } } });
          const fields = { search: { gather: { enabled: true, decision, ...limits } } };
          f.project(harness ? { harnesses: { pi: fields } } : fields);
          expect(loadAftConfig(f.root).search?.gather).toEqual({ enabled: true, max_nodes: 7 });
          const warnings = warn.mock.calls.map((call) => call[0]).join("\n");
          for (const key of ["enabled", ...Object.keys(limits)])
            expect(warnings).toContain(`search.gather.${key}`);
          for (const key of Object.keys(decision))
            expect(warnings).toContain(`search.gather.decision.${key}`);
        }
      } finally {
        warn.mockRestore();
      }
    });
  });
  test("configure projection preserves gather and rerank through the public loader", async () => {
    await fixture((f) => {
      const gather = { enabled: true, decision, ...limits };
      f.user({ search: { gather, rerank: { backend: "onnx", top_n: 5 } } });
      f.project({ search: { rerank: { backend: "off" } } });
      expect(resolveProjectOverridesForConfigure(loadAftConfig(f.root)).search).toEqual({
        gather,
        rerank: { backend: "off", top_n: 5 },
      });
      f.project({ search: { gather: { enabled: false } } });
      expect(resolveProjectOverridesForConfigure(loadAftConfig(f.root)).search).toEqual({
        gather: { ...gather, enabled: false },
        rerank: { backend: "onnx", top_n: 5 },
      });
    });
  });
  test("trusted harness fields preserve explicit enablement and rerank", async () => {
    await fixture((f) => {
      f.user({
        search: { gather: { enabled: true, decision }, rerank: { backend: "onnx" } },
        harnesses: { pi: { search: { gather: { max_nodes: 9 } } } },
      });
      f.project({});
      expect(resolveProjectOverridesForConfigure(loadAftConfig(f.root)).search).toEqual({
        gather: { enabled: true, decision, max_nodes: 9 },
        rerank: { backend: "onnx" },
      });
    });
  });
  test("endpoint alone does not enable gather", async () => {
    await fixture((f) => {
      f.user({ search: { gather: { decision } } });
      f.project({});
      const config = loadAftConfig(f.root);
      expect(config.search?.gather?.enabled === true).toBe(false);
      expect(resolveProjectOverridesForConfigure(config).search).toEqual({ gather: { decision } });
    });
  });
  test("partial harness decision overrides inherit the other trusted fields", async () => {
    await fixture((f) => {
      f.user({
        search: { gather: { enabled: true, decision } },
        harnesses: { pi: { search: { gather: { decision: { api_key_env: "HARNESS_KEY" } } } } },
      });
      f.project({});
      const config = loadAftConfig(f.root);
      const inherited = { ...decision, api_key_env: "HARNESS_KEY" };
      expect(config.search?.gather?.decision).toEqual(inherited);
      expect(resolveProjectOverridesForConfigure(config).search).toEqual({
        gather: { enabled: true, decision: inherited },
      });
    });
  });
  test("user harness search keeps rerank siblings but replaces an explicit rerank block", async () => {
    await fixture((f) => {
      const rerank = {
        backend: "remote",
        model: "base-model",
        endpoint: "https://example.test/rerank",
        api_key_env: "BASE_KEY",
        top_n: 7,
        timeout_ms: 500,
      };
      for (const override of [
        { gather: { enabled: true } },
        { gather: { enabled: true }, rerank: { backend: "off" } },
        { gather: { enabled: true }, rerank: {} },
      ]) {
        f.user({ search: { rerank }, harnesses: { pi: { search: override } } });
        f.project({});
        const config = loadAftConfig(f.root);
        const expected = "rerank" in override ? override.rerank : rerank;
        expect(config.search?.rerank).toEqual(expected);
        expect(resolveProjectOverridesForConfigure(config).search).toEqual({
          gather: { enabled: true },
          ...(expected && Object.keys(expected).length > 0 ? { rerank: expected } : {}),
        });
      }
    });
  });
  test("empty decisions are omitted from user and harness config and configure projection", async () => {
    expect(
      resolveProjectOverridesForConfigure({
        disabled_tools: [],
        search: { gather: { enabled: true, decision: {} } },
      }).search,
    ).toEqual({ gather: { enabled: true } });
    await fixture((f) => {
      for (const user of [
        { search: { gather: { enabled: true, decision: {} } } },
        { harnesses: { pi: { search: { gather: { enabled: true, decision: {} } } } } },
        {
          search: { gather: { enabled: true, decision: {} } },
          harnesses: { pi: { search: { gather: { decision: {} } } } },
        },
      ]) {
        f.user(user);
        f.project({});
        const config = loadAftConfig(f.root);
        expect(config.search?.gather?.decision).toBeUndefined();
        expect(resolveProjectOverridesForConfigure(config).search).toEqual({
          gather: { enabled: true },
        });
      }
      f.user({
        search: { gather: { enabled: true, decision } },
        harnesses: { pi: { search: { gather: { decision: {} } } } },
      });
      expect(loadAftConfig(f.root).search?.gather?.decision).toEqual(decision);
    });
  });
  test("project base disables survive an active harness enable attempt", async () => {
    await fixture((f) => {
      f.user({ search: { gather: { enabled: true }, rerank: { backend: "onnx" } } });
      f.project({
        search: { gather: { enabled: false }, rerank: { backend: "off" } },
        harnesses: { pi: { search: { gather: { enabled: true }, rerank: { backend: "remote" } } } },
      });
      expect(loadAftConfig(f.root).search).toMatchObject({
        gather: { enabled: false },
        rerank: { backend: "off" },
      });
    });
  });
  test("gather schema rejects invalid integers, strings, and malformed project values", async () => {
    for (const key of Object.keys(limits)) {
      for (const value of [0, -1, 1.5, "3"])
        expect(AftConfigSchema.safeParse({ search: { gather: { [key]: value } } }).success).toBe(
          false,
        );
    }
    for (const key of ["endpoint", "model", "api_key_env"])
      expect(
        AftConfigSchema.safeParse({ search: { gather: { decision: { [key]: "  " } } } }).success,
      ).toBe(false);
    for (const gather of [false, [], { enabled: "true" }, { decision: { backend: "local" } }]) {
      expect(AftConfigSchema.safeParse({ search: { gather } }).success).toBe(false);
      await fixture((f) => {
        f.user({});
        f.project({ search: { gather } });
        expect(() => loadAftConfig(f.root)).toThrow("invalid_security_config:search");
      });
    }
  });
  test("live user changes and project disable reach the next config consumer call", async () => {
    await fixture((f) => {
      f.user({ search: { gather: { enabled: false } } });
      f.project({});
      let config = loadAftConfig(f.root);
      const nextCall = () => resolveProjectOverridesForConfigure(config).search;
      const reload = startPiLiveConfigReload({
        directory: f.root,
        initialSources: [...getConfigLoadSources()],
        initialSourceTexts: Object.fromEntries(getConfigLoadTexts()),
        getConfig: () => config,
        setConfig: (next) => {
          config = next;
        },
        notify: () => {},
        watch: false,
      });
      try {
        f.user({ search: { gather: { enabled: true, decision } } });
        expect(reload.reload()?.applied).toContain("search.gather");
        expect(nextCall()).toEqual({ gather: { enabled: true, decision } });
        f.project({ search: { gather: { enabled: false } } });
        expect(reload.reload()?.applied).toContain("search.gather");
        expect(nextCall()).toEqual({ gather: { enabled: false, decision } });
      } finally {
        reload.stop();
      }
    });
  });
});
