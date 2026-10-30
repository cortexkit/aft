import { afterEach, describe, expect, spyOn, test } from "bun:test";
import { Database, SQLiteError } from "bun:sqlite";
import { createHash, randomUUID } from "node:crypto";
import {
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rename,
  rm,
  stat,
  symlink,
  utimes,
  writeFile,
} from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { pathToFileURL } from 'node:url';

import { mapWithConcurrency, parseE2EConcurrency } from "./concurrency.js";
import {
  assertV1HostEditFamilyGate,
  type HostCliContract,
  loadHostCliContract,
  loadHostProviderConfigContract,
  loadHostSchemaRejectionContract,
  loadV1HostEditFamilyGateContract,
  loadV1HostProviderConfigContract,
} from "./contracts.js";
import {
  assertThreeStateRestore,
  DiskStateObserver,
  snapshotPaths,
  type PathState,
} from "./disk-state.js";
import { HarnessError, type HarnessFailureCode } from "./errors.js";
import { parseArtifactRetention, pruneOldRunRoots } from "./forensics.js";
import {
  awaitControlToolCall,
  findToolCalled,
  HostEventRecorder,
  type HostEvent,
} from "./event-stream.js";
import { runApiControl, startScenarioClient, startSharedServer } from "./host.js";
import { readPermissionAskInventory } from "./inventory.js";
import { createScenarioIsolation } from "./isolation.js";
import {
  assertT5HostWakeTranscript,
  CompletionWakeLiveness,
  WatchPatternLiveness,
} from "./liveness.js";
import {
  assertRequestToolSurface,
  hostToolArguments,
  hostToolName,
  isTitleGenerationRequest,
  materializeTurnPlaceholders,
  observeThenRespond,
  toolResultForCall,
} from "./mock-server.js";
import { taskKillDeadlineText } from "../../../../packages/aft-bridge/src/bash-hints.js";
import { cachedExecutable as cacheSharedExecutable } from "../../../../packages/aft-bridge/src/__tests__/test-utils/cached-executable.js";
import {
  assertComparison,
  assertDualHostParity,
  assertT6Trailer,
  describeTextDifference,
  projectText,
  TRUNCATION_TRAILER_PATTERN,
} from "./projection.js";
import {
  assertPermissionPromptObserved,
  assertV2ToolsAreAft,
  controlPlans,
  permissionAction,
  sessionPermissionRules,
} from "./permission-plan.js";
import {
  callAbortedFailure,
  ProcessObserver,
  processGroupRunning,
  type TaskProbe,
  type TaskState,
  waitForAbortSettlement,
  waitForTaskStatus,
} from "./process-observer.js";
import { readPinnedV1HostVersion } from "./pin.js";
import { AftTaskProbe } from "./task-probe.js";
import {
  awaitTurnReadiness,
  CALLGRAPH_READY_TIMEOUT_MS,
  callgraphStorePublished,
  readinessBudgetMs,
} from "./readiness.js";
import { parentDisposition, reportTable } from "./report.js";
import { verifyExecutableProvenance } from "./provenance.js";
import {
  addCallgraphWarmup,
  loadScenarios,
  materializeParityScenarios,
  validateScenarioDefinition,
} from "./scenario-loader.js";
import { assertTurnLog } from "./turn-log.js";
import {
  deadTransportStubSource,
  makeTransportDeadStub,
  pointTransportDeadStub,
} from "./transport-stub.js";
import { resolveTransportDeadWindow, transportDeadAtTurn } from "./transport-window.js";
import type {
  ApiControlPlan,
  ScenarioDefinition,
  ScenarioResult,
  ScriptedTurn,
  ToolCallPlan,
} from "./types.js";
import {
  applyMutatingTestOverride,
  deriveListSurfaces,
  deriveMutatingTools,
  deriveV2HarnessProjection,
  loadApplicabilityMatrix,
  loadToolSchemas,
  validateHarnessInputs,
  validateParityAllowlist,
  validateT6,
  validateInventory,
  validateMutatingDeclarations,
} from "./validation.js";
import { sha256File } from "./util.js";

const roots: string[] = [];

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

async function root(): Promise<string> {
  const path = await mkdtemp(join(tmpdir(), "opencode2-harness-test-"));
  roots.push(path);
  return path;
}

// Executable fixtures use the bridge's immutable content-addressed cache so
// identical bytes share one launch path across test runs. Keep non-executable
// fixture data in its own small cache instead of the per-test temp directories.
const CACHED_DATA_FILES = join(tmpdir(), "opencode2-harness-test-data");

async function cachedFile(content: string, executable: boolean): Promise<string> {
  if (executable) return cacheSharedExecutable(content);

  const digest = createHash("sha256")
    .update("data\0")
    .update(content)
    .digest("hex")
    // Short enough that a path to it still fits in a `#!` line on Linux.
    .slice(0, 24);
  const directory = join(CACHED_DATA_FILES, digest);
  const path = join(directory, "data");
  const existing = await stat(path).catch(() => undefined);
  if (existing?.isFile() && (await readFile(path, "utf8")) === content) return path;

  // Written under a unique name and renamed into place, so a concurrent run
  // either sees no file or the whole file, never a half-written one.
  await mkdir(directory, { recursive: true });
  const pending = join(directory, `.pending-${process.pid}-${randomUUID()}`);
  await writeFile(pending, content);
  await rename(pending, path);
  return path;
}

function cachedExecutable(content: string): Promise<string> {
  return cachedFile(content, true);
}

const CAPTURE_ARGUMENTS_SCRIPT = '#!/bin/sh\nprintf "%s\\n" "$@" > "$ARGUMENTS_PATH"\n';

function call(overrides: Partial<ToolCallPlan> = {}): ToolCallPlan {
  return {
    id: "call-1",
    name: "write",
    arguments: { filePath: "changed.txt", content: "changed\n" },
    ...overrides,
  };
}

function scenario(toolCall: ToolCallPlan): ScenarioDefinition {
  return {
    schema_version: 1,
    id: "write/T1/control",
    tool: "write",
    trajectory: "T1",
    execution: "standalone",
    prompt: "control",
    turns: [
      {
        label: "turn-1",
        response: { kind: "tool_calls", calls: [toolCall] },
      },
    ],
  };
}

describe("bounded scenario concurrency", () => {
  test("defaults to four and rejects invalid worker counts", () => {
    expect(parseE2EConcurrency(undefined)).toBe(4);
    expect(parseE2EConcurrency("1")).toBe(1);
    for (const invalid of ["0", "-1", "1.5", "not-a-number"]) {
      expect(() => parseE2EConcurrency(invalid)).toThrow("must be a positive integer");
    }
  });

  test("caps active work and preserves input order instead of completion order", async () => {
    const gates = Array.from({ length: 4 }, () => {
      let release = () => {};
      const promise = new Promise<void>((resolve) => {
        release = resolve;
      });
      return { promise, release };
    });
    const started: number[] = [];
    const completed: number[] = [];
    let active = 0;
    let maximumActive = 0;
    const running = mapWithConcurrency(["zero", "one", "two", "three"], 2, async (value, index) => {
      started.push(index);
      active += 1;
      maximumActive = Math.max(maximumActive, active);
      await gates[index].promise;
      active -= 1;
      completed.push(index);
      return value;
    });

    while (started.length < 2) await Bun.sleep(1);
    expect(started).toEqual([0, 1]);
    gates[1].release();
    while (started.length < 3) await Bun.sleep(1);
    gates[2].release();
    while (started.length < 4) await Bun.sleep(1);
    gates[3].release();
    gates[0].release();

    expect(await running).toEqual(["zero", "one", "two", "three"]);
    expect(completed).toEqual([1, 2, 3, 0]);
    expect(maximumActive).toBe(2);
  });
});

describe("AFT task database probe", () => {
  const expectedStates = [
    { id: "task-1", status: "completed", status_reason: "exited", pgid: undefined },
  ];

  function createWriter(databasePath: string): Database {
    const writer = new Database(databasePath, { create: true, strict: true });
    writer.exec(`
        PRAGMA journal_mode = WAL;
        PRAGMA wal_autocheckpoint = 0;
        CREATE TABLE bash_tasks (
          task_id TEXT PRIMARY KEY, status TEXT, pgid INTEGER, metadata TEXT,
          harness TEXT, started_at INTEGER
        );
        INSERT INTO bash_tasks VALUES (
          'task-1', 'completed', NULL, '{"status_reason":"exited"}', 'opencode', 1
        );
      `);
    return writer;
  }

  function sqliteError(code: string, errno: number): SQLiteError {
    const database = new Database(":memory:");
    try {
      database.exec("PRAGMA query_only = ON; CREATE TABLE readonly_failure (id INTEGER)");
    } catch (error) {
      if (error instanceof SQLiteError) {
        // Bun constructs SQLiteError internally and its code fields are read-only.
        return Object.create(error, { code: { value: code }, errno: { value: errno } });
      }
      throw error;
    } finally {
      database.close();
    }
    throw new Error("SQLite did not reject a write to a query-only database");
  }

  test("reads a closed WAL database without existing sidecars", async () => {
    const directory = await root();
    const databasePath = join(directory, "aft.db");
    createWriter(databasePath).close();
    expect(await readdir(directory)).toEqual(["aft.db"]);
    expect(await new AftTaskProbe(databasePath).states()).toEqual(expectedStates);
  });

  test("reads committed WAL frames left by a killed writer", async () => {
    const directory = await root();
    const databasePath = join(directory, "aft.db");
    createWriter(databasePath).close();
    const writer = Bun.spawnSync([
      process.execPath,
      "-e",
      `import { Database } from "bun:sqlite";
       const db = new Database(${JSON.stringify(databasePath)});
       db.exec("PRAGMA wal_autocheckpoint = 0; UPDATE bash_tasks SET status = 'running'");
       process.kill(process.pid, "SIGKILL");`,
    ]);
    expect(writer.signalCode).toBe("SIGKILL");
    expect((await stat(`${databasePath}-wal`)).size).toBeGreaterThan(32);
    expect(await readdir(directory)).toContain("aft.db-shm");
    expect(await new AftTaskProbe(databasePath).states()).toEqual([
      { ...expectedStates[0], status: "running" },
    ]);
  });

  test("reads committed tasks while a live writer holds a transaction", async () => {
    const databasePath = join(await root(), "aft.db");
    const writer = createWriter(databasePath);
    try {
      writer.exec("BEGIN IMMEDIATE; UPDATE bash_tasks SET status = 'running'");
      expect(await new AftTaskProbe(databasePath).states()).toEqual(expectedStates);
      expect(writer.inTransaction).toBe(true);
    } finally {
      writer.close();
    }
  });

  test("retries read-only WAL recovery once with a read-write connection", async () => {
    const databasePath = join(await root(), "aft.db");
    createWriter(databasePath).close();
    // A writable directory can let SQLite create sidecars even on a read-only
    // connection. Inject the recovery error to exercise the retry on every host.
    const recoveryError = sqliteError("SQLITE_READONLY_RECOVERY", 264);
    const open = spyOn(Database, "open");
    const exec = spyOn(Database.prototype, "exec");
    const close = spyOn(Database.prototype, "close");
    const query = spyOn(Database.prototype, "query").mockImplementationOnce(() => {
      throw recoveryError;
    });
    try {
      expect(await new AftTaskProbe(databasePath).states()).toEqual(expectedStates);
      expect(open.mock.calls).toEqual([
        [databasePath, { readonly: true, readwrite: false, create: false, strict: true }],
        [databasePath, { readonly: false, readwrite: true, create: false, strict: true }],
      ]);
      expect(exec.mock.calls).toEqual([
        ["PRAGMA busy_timeout = 5000"],
        ["PRAGMA busy_timeout = 5000"],
      ]);
      expect(close).toHaveBeenCalledTimes(2);
    } finally {
      query.mockRestore();
      close.mockRestore();
      exec.mockRestore();
      open.mockRestore();
    }
  });

  test.each([
    ["SQLITE_READONLY", 8],
    ["SQLITE_READONLY_CANTINIT", 1288],
    ["SQLITE_CANTOPEN", 14],
  ] as const)("retries an open failure for %s", async (code, errno) => {
    const databasePath = join(await root(), "aft.db");
    createWriter(databasePath).close();
    const error = sqliteError(code, errno);
    const open = spyOn(Database, "open").mockImplementationOnce(() => {
      throw error;
    });
    try {
      expect(await new AftTaskProbe(databasePath).states()).toEqual(expectedStates);
      expect(open).toHaveBeenCalledTimes(2);
      expect(open.mock.calls[1]).toEqual([
        databasePath, { readonly: false, readwrite: true, create: false, strict: true },
      ]);
    } finally {
      open.mockRestore();
    }
  });

  test("a failed read-write retry still throws and closes both connections", async () => {
    const databasePath = join(await root(), "aft.db");
    createWriter(databasePath).close();
    const error = sqliteError("SQLITE_CANTOPEN", 14);
    const close = spyOn(Database.prototype, "close");
    const query = spyOn(Database.prototype, "query").mockImplementation(() => {
      throw error;
    });
    try {
      await expect(new AftTaskProbe(databasePath).states()).rejects.toBe(error);
      expect(query).toHaveBeenCalledTimes(2);
      expect(close).toHaveBeenCalledTimes(2);
    } finally {
      query.mockRestore();
      close.mockRestore();
    }
  });

  test.each([
    ["SQLITE_BUSY", 5],
    ["SQLITE_IOERR", 10],
    ["SQLITE_CORRUPT", 11],
  ] as const)("does not retry %s", async (code, errno) => {
    const databasePath = join(await root(), "aft.db");
    createWriter(databasePath).close();
    const error = sqliteError(code, errno);
    const query = spyOn(Database.prototype, "query").mockImplementationOnce(() => {
      throw error;
    });
    try {
      await expect(new AftTaskProbe(databasePath).states()).rejects.toBe(error);
      expect(query).toHaveBeenCalledTimes(1);
    } finally {
      query.mockRestore();
    }
  });

  test("does not retry a non-SQLite error with a readonly code", async () => {
    const databasePath = join(await root(), "aft.db");
    createWriter(databasePath).close();
    const error = Object.assign(new Error("not from SQLite"), { errno: 8 });
    const query = spyOn(Database.prototype, "query").mockImplementationOnce(() => {
      throw error;
    });
    try {
      await expect(new AftTaskProbe(databasePath).states()).rejects.toBe(error);
      expect(query).toHaveBeenCalledTimes(1);
    } finally {
      query.mockRestore();
    }
  });

  test("a missing task database is not created", async () => {
    const directory = await root();
    expect(await new AftTaskProbe(join(directory, "aft.db")).states()).toEqual([]);
    expect(await readdir(directory)).toEqual([]);
  });

  test("a corrupt task database still throws", async () => {
    const databasePath = join(await root(), "aft.db");
    await writeFile(databasePath, "not a SQLite database");
    await expect(new AftTaskProbe(databasePath).states()).rejects.toMatchObject({
      code: "SQLITE_NOTADB",
    });
  });
});

describe("task readiness conditions", () => {
  test("waits until a task reaches the requested state", async () => {
    let reads = 0;
    const probe: TaskProbe = {
      states: async () => {
        reads += 1;
        if (reads === 1) throw new Error("database is locked");
        return reads === 2 ? [] : [{ id: "bash-ready", status: "running", pgid: 42 }];
      },
    };

    const evidence = await waitForTaskStatus(probe, "running", 1_000);

    expect(evidence.task).toEqual({ id: "bash-ready", status: "running", pgid: 42 });
    expect(evidence.observed).toEqual([evidence.task]);
    expect(evidence.probe_errors).toBe(1);
    expect(evidence.last_probe_error).toBe("database is locked");
    expect(reads).toBe(3);
  });

  test("a timeout reports the task states it actually observed", async () => {
    const probe: TaskProbe = {
      states: async () => [{ id: "bash-finished", status: "completed" }],
    };

    await expect(waitForTaskStatus(probe, "running", 1)).rejects.toThrow(
      'did not observe status "running" within 1ms; observed [{"id":"bash-finished","status":"completed"}]; probe errors 0',
    );
  });
});

async function expectCode(
  action: () => Promise<unknown>,
  code: HarnessFailureCode,
): Promise<HarnessError> {
  try {
    await action();
  } catch (error) {
    expect(error).toBeInstanceOf(HarnessError);
    expect((error as HarnessError).code).toBe(code);
    return error as HarnessError;
  }
  throw new Error(`expected ${code}`);
}

describe("scenario isolation and liveness", () => {
  test("all host roots are private and the plugin source is a tarball", async () => {
    const parent = await root();
    const fixture = join(parent, "fixture");
    await mkdir(fixture);
    await writeFile(join(fixture, "file.txt"), "fixture\n");
    const tarball = join(parent, "plugin.tgz");
    await writeFile(tarball, "pack");
    const pluginDirectory = join(parent, "installed-plugin");
    await mkdir(join(pluginDirectory, "dist", "entry"), { recursive: true });
    await writeFile(join(pluginDirectory, "dist", "index.js"), "export default {};\n");
    await writeFile(join(pluginDirectory, "dist", "entry", "server.js"), "export default {};\n");
    const isolated = await createScenarioIsolation({
      parent: join(parent, "runs"),
      scenarioId: "read/T1/happy",
      scenarioEnv: { AFT_TEST_INSPECT_DELAY_MS: "30000", HOME: "/not-isolated" },
      fixture,
      pluginTarball: tarball,
      pluginDirectory,
      pluginVersion: "1.2.3-test",
      hostGeneration: "v2",
      binaryPath: "/native/aft",
      mockBaseUrl: "http://127.0.0.1:1234",
      providerConfig: {
        mock: {
          package: "observed-package",
          settings: { baseURL: "{{AIMOCK_BASE_URL}}/v1" },
          models: { "mock-model": { name: "Mock" } },
        },
      },
      providerConfigKey: "providers",
    });
    for (const key of [
      "HOME",
      "XDG_CONFIG_HOME",
      "XDG_DATA_HOME",
      "XDG_STATE_HOME",
      "XDG_CACHE_HOME",
      "XDG_RUNTIME_DIR",
    ]) {
      expect(isolated.env[key]?.startsWith(isolated.root)).toBe(true);
    }
    expect(isolated.env.OPENCODE_DISABLE_DEFAULT_PLUGINS).toBe("true");
    expect(isolated.env.OPENCODE_DB).toBe("opencode2.db");
    expect(isolated.env.AFT_BINARY_PATH).toBe("/native/aft");
    expect(isolated.env.AFT_TEST_INSPECT_DELAY_MS).toBe("30000");
    expect(process.env.AFT_TEST_INSPECT_DELAY_MS).not.toBe("30000");
    const aftConfig = JSON.parse(
      await readFile(join(isolated.project, ".cortexkit", "aft.jsonc"), "utf8"),
    );
    expect(aftConfig).toEqual({});
    const userAftConfig = JSON.parse(
      await readFile(join(isolated.config, "cortexkit", "aft.jsonc"), "utf8"),
    );
    expect(userAftConfig).toEqual({ disabled_tools: [] });
    const hostConfig = JSON.parse(await readFile(isolated.host_config, "utf8"));
    // A V2 host reads `plugins`, configured the way `aft setup` writes it: AFT's
    // entry and removals for both host plugins AFT replaces.
    expect(hostConfig.plugin).toBeUndefined();
    expect(hostConfig.plugins[0]).toEndWith("/xdg-config/aft-opencode-wrapper");
    expect(hostConfig.plugins.slice(1)).toEqual(["-opencode.tool.shell", "-opencode.tool.patch"]);
    expect(hostConfig.providers.mock.settings.baseURL).toBe("http://127.0.0.1:1234/v1");
    expect(hostConfig.provider).toBeUndefined();
    const serverWrapper = await readFile(
      join(isolated.config, "aft-opencode-wrapper", "index.mjs"),
      "utf8",
    );
    expect(serverWrapper).toContain(pathToFileURL(tarball).href);
    expect(serverWrapper).toContain("dist/entry/server.js");
    expect(serverWrapper).toContain("PLUGIN_VERSION=1.2.3-test");

    const legacy = await createScenarioIsolation({
      parent: join(parent, "legacy-runs"),
      scenarioId: "read/T7/legacy",
      userConfig: { bash: { changed_files: "diff" } },
      fixture,
      pluginTarball: tarball,
      pluginDirectory,
      pluginVersion: "1.2.3-test",
      hostGeneration: "v1",
      mockBaseUrl: "http://127.0.0.1:1234",
      providerConfig: {
        mock: {
          api: "openai",
          options: { baseURL: "{{AIMOCK_BASE_URL}}/v1" },
          models: { "mock-model": { name: "Mock" } },
        },
      },
      providerConfigKey: "provider",
    });
    const legacyConfig = JSON.parse(await readFile(legacy.host_config, "utf8"));
    expect(JSON.parse(await readFile(join(legacy.config, "cortexkit", "aft.jsonc"), "utf8")))
      .toEqual({ disabled_tools: [], bash: { changed_files: "diff" } });
    expect(JSON.parse(await readFile(join(legacy.project, ".cortexkit", "aft.jsonc"), "utf8")))
      .toEqual({});
    // The V1 host reads `provider`; it logs `providers` as an unsupported key
    // and then has no provider to resolve the run's model against.
    expect(legacyConfig.provider.mock.options.baseURL).toBe("http://127.0.0.1:1234/v1");
    expect(legacyConfig.providers).toBeUndefined();
    const legacyWrapper = await readFile(
      join(legacy.config, "aft-opencode-wrapper", "index.mjs"),
      "utf8",
    );
    expect(legacyWrapper).toContain("dist/index.js");
    expect(legacyWrapper).not.toContain("dist/entry/server.js");
    // V1 takes a plugin function, not the V2 object carrying `effect`.
    expect(legacyWrapper).toContain("export default (context) => {");
    expect(legacyWrapper).not.toContain("plugin.effect");
    expect(serverWrapper).toContain("plugin.effect");
  });

  test("a V1 config directory starts with the host's own dependency install already done", async () => {
    const parent = await root();
    const tarball = join(parent, "plugin.tgz");
    await writeFile(tarball, "pack");
    const pluginDirectory = join(parent, "installed-plugin");
    await mkdir(join(pluginDirectory, "dist", "entry"), { recursive: true });
    await writeFile(join(pluginDirectory, "dist", "index.js"), "export default {};\n");
    await writeFile(join(pluginDirectory, "dist", "entry", "server.js"), "export default {};\n");
    // The files `npm install --save-exact @opencode-ai/plugin@<version>` leaves
    // behind: a manifest, a lock whose root lists the package, and the package.
    const template = join(parent, "config-deps");
    await mkdir(join(template, "node_modules", "@opencode-ai", "plugin"), { recursive: true });
    await writeFile(
      join(template, "node_modules", "@opencode-ai", "plugin", "package.json"),
      `${JSON.stringify({ name: "@opencode-ai/plugin", version: "1.18.30" })}\n`,
    );
    const manifest = { dependencies: { "@opencode-ai/plugin": "1.18.30" } };
    await writeFile(join(template, "package.json"), `${JSON.stringify(manifest)}\n`);
    await writeFile(
      join(template, "package-lock.json"),
      `${JSON.stringify({ name: "opencode", lockfileVersion: 3, packages: { "": manifest } })}\n`,
    );
    const isolate = (hostGeneration: "v1" | "v2", hostDependencies: string, id: string) =>
      createScenarioIsolation({
        parent: join(parent, "runs"),
        scenarioId: id,
        pluginTarball: tarball,
        pluginDirectory,
        pluginVersion: "1.2.3-test",
        hostGeneration,
        mockBaseUrl: "http://127.0.0.1:1234",
        providerConfig: {},
        providerConfigKey: hostGeneration === "v1" ? "provider" : "providers",
        hostDependencies,
      });

    const legacy = await isolate("v1", template, "bash/T7/happy");
    const configDirectory = join(legacy.config, "opencode");
    // The V1 host skips its per-start npm install only when all of this holds
    // (opencode-ai 1.18.30, Npm.install): node_modules exists, and the lock's
    // root lists every dependency of package.json plus the package it adds.
    expect((await stat(join(configDirectory, "node_modules"))).isDirectory()).toBe(true);
    expect(
      await readFile(
        join(configDirectory, "node_modules", "@opencode-ai", "plugin", "package.json"),
        "utf8",
      ),
    ).toContain("1.18.30");
    const seededManifest = JSON.parse(await readFile(join(configDirectory, "package.json"), "utf8"));
    const seededLock = JSON.parse(
      await readFile(join(configDirectory, "package-lock.json"), "utf8"),
    );
    const locked = Object.keys(seededLock.packages[""].dependencies);
    for (const name of [...Object.keys(seededManifest.dependencies), "@opencode-ai/plugin"]) {
      expect(locked).toContain(name);
    }
    // Seeding does not displace the opencode.json the harness writes into the
    // same directory.
    expect(JSON.parse(await readFile(legacy.host_config, "utf8")).provider).toEqual({});

    // V2 does not run that install, so its config directory is left alone.
    const current = await isolate("v2", template, "bash/T1/happy");
    expect(await readdir(join(current.config, "opencode"))).toEqual(["opencode.json"]);

    // An image whose install is incomplete would bring the slow start back
    // without a word; it is refused instead.
    await rm(join(template, "node_modules"), { recursive: true });
    await expectCode(() => isolate("v1", template, "bash/T7/broken"), "host_failed");
  });

  test("the scenario client uses the provider contract model", async () => {
    const parent = await root();
    const executable = await cachedExecutable(CAPTURE_ARGUMENTS_SCRIPT);
    const argumentsPath = join(parent, "arguments.txt");

    const client = startScenarioClient({
      executable,
      scenario: scenario(call()),
      cwd: parent,
      env: { ...process.env, ARGUMENTS_PATH: argumentsPath },
      processObserver: { trackChild() {} } as never,
      hostGeneration: "v2",
      model: "openai/mock-model",
    });
    expect((await client.wait()).exit_code).toBe(0);
    const args = (await readFile(argumentsPath, "utf8")).trim().split("\n");
    expect(args).toContain("--print-logs");
    expect(args.slice(args.indexOf("--model"), args.indexOf("--model") + 2)).toEqual([
      "--model",
      "openai/mock-model",
    ]);
  });

  test("V2 scenario model overrides the provider contract default without changing the V1 leg", async () => {
    const parent = await root();
    const executable = await cachedExecutable(CAPTURE_ARGUMENTS_SCRIPT);
    for (const hostGeneration of ["v2", "v1"] as const) {
      const argumentsPath = join(parent, `${hostGeneration}-arguments.txt`);
      const client = startScenarioClient({
        executable,
        scenario: { ...scenario(call()), model: "openai/gpt-5-mock" },
        cwd: parent,
        env: { ...process.env, ARGUMENTS_PATH: argumentsPath },
        processObserver: { trackChild() {} } as never,
        hostGeneration,
        model: hostGeneration === "v2" ? "openai/mock-model" : "mock/mock-model",
      });
      expect((await client.wait()).exit_code).toBe(0);
      const args = (await readFile(argumentsPath, "utf8")).trim().split("\n");
      expect(args.slice(args.indexOf("--model"), args.indexOf("--model") + 2)).toEqual([
        "--model",
        hostGeneration === "v2" ? "openai/gpt-5-mock" : "mock/mock-model",
      ]);
    }
  });

  test("every apply_patch scenario including materialized T7 selects the registered GPT mock", async () => {
    const scenarios = materializeParityScenarios(await loadScenarios(join(import.meta.dir, "../scenarios")))
      .filter((entry) => entry.tool === "apply_patch");
    expect(scenarios).toHaveLength(7);
    for (const entry of scenarios) expect(entry.model).toBe("openai/gpt-5-mock");
    const contract = await loadHostProviderConfigContract(join(import.meta.dir, "../contract"), "2.1.0");
    const openai = contract.provider_config.openai as Record<string, unknown>;
    expect(Object.keys(openai.models as Record<string, unknown>).sort()).toEqual(["gpt-5-mock", "mock-model"]);
    expect((openai.settings as Record<string, unknown>).baseURL).toBe("{{AIMOCK_BASE_URL}}/v1");
  });

  test("checked-in OpenCode 2.1.0 contracts match the matrix pin", async () => {
    const contractRoot = join(import.meta.dir, "../contract");
    const cli = await loadHostCliContract(contractRoot, "2.1.0");
    const provider = await loadHostProviderConfigContract(contractRoot, "2.1.0");
    const schema = await loadHostSchemaRejectionContract(contractRoot, "2.1.0");

    expect(cli.host_version).toBe("2.1.0");
    expect(provider.host_version).toBe("2.1.0");
    expect(provider.config_key).toBe("providers");
    expect(provider.model).toBe("openai/mock-model");
    expect(schema.host_version).toBe("2.1.0");
    expect(schema.json_event.state_metadata).toEqual({
      providerCall: { executed: false },
      providerResult: { executed: false },
      rawInput: "{}",
    });
  });

  // The surface check runs on the V2 leg, where AFT's apply_patch is `patch`.
  test("non-GPT tool-surface scenario rejects a leaked patch", async () => {
    const cases = await loadScenarios(join(import.meta.dir, "../scenarios"));
    const selected = cases.find((entry) => entry.id === "read/T1/edit-family/non-gpt")!;
    expect(selected.model).toBe("openai/mock-model");
    const expected = selected.metadata!.tool_surface as { model: string; present: string[]; absent: string[] };
    const exchange = {
      index: 0, label: "call-read", response: {}, observed_at: "test",
      request: { model: "mock-model", tools: ["read", "edit", "write", "patch"].map((name) => ({ type: "function", function: { name } })) },
    };
    expect(() => assertRequestToolSurface(exchange, expected)).toThrow("unexpectedly includes patch");
  });

  test("GPT tool-surface scenario checks actual tool names and model", async () => {
    const cases = await loadScenarios(join(import.meta.dir, "../scenarios"));
    const selected = cases.find((entry) => entry.id === "read/T1/edit-family/gpt")!;
    expect(selected.model).toBe("openai/gpt-5-mock");
    const expected = selected.metadata!.tool_surface as { model: string; present: string[]; absent: string[] };
    const exchange = {
      index: 0, label: "call-read", response: {}, observed_at: "test",
      request: { model: "gpt-5-mock", tools: ["read", "patch"].map((name) => ({ type: "function", function: { name } })) },
    };
    expect(assertRequestToolSurface(exchange, expected)).toEqual(["patch", "read"]);
    expect(() => assertRequestToolSurface({ ...exchange, request: { model: "mock-model", tools: exchange.request.tools } }, expected)).toThrow("expected gpt-5-mock");
    expect(() => assertRequestToolSurface({ ...exchange, request: { model: "gpt-5-mock" } }, expected)).toThrow("contains no tools");
    expect(() => assertRequestToolSurface({ ...exchange, request: { model: "gpt-5-mock", tools: [{ function: { name: "read" } }] } }, expected)).toThrow("missing patch");
    // AFT's OpenCode 1 name is not what an OpenCode 2 host offers.
    expect(() => assertRequestToolSurface({ ...exchange, request: { model: "gpt-5-mock", tools: ["read", "patch", "apply_patch"].map((name) => ({ type: "function", function: { name } })) } }, expected)).toThrow("unexpectedly includes apply_patch");
  });

  describe("V2 requests offer AFT's shell and patch, never the host's", () => {
    const scenario = { id: "bash/T1/sample" } as never;
    const offering = (tools: Array<{ name: string; description?: string }>) => [
      {
        index: 0,
        label: "call",
        response: {},
        observed_at: "test",
        request: { tools: tools.map((fn) => ({ type: "function", function: fn })) },
      },
    ];

    test("AFT's own shell and patch pass", () => {
      expect(() =>
        assertV2ToolsAreAft(
          scenario,
          offering([
            { name: "shell", description: "Execute shell commands. Output is compressed" },
            { name: "patch", description: "Use the `patch` tool to edit files. Your patch" },
            { name: "read", description: "Read a file" },
          ]),
        ),
      ).not.toThrow();
    });

    test("the host's own shell is caught by its description", () => {
      expect(() =>
        assertV2ToolsAreAft(
          scenario,
          offering([{ name: "shell", description: "Execute a shell command and return its output." }]),
        ),
      ).toThrow("the host offered its own shell tool although -opencode.tool.shell is configured");
    });

    test("the host's own patch is caught by its description", () => {
      expect(() =>
        assertV2ToolsAreAft(scenario, offering([{ name: "patch", description: "Apply a patch." }])),
      ).toThrow("the host offered its own patch tool although -opencode.tool.patch is configured");
    });

    test("AFT's OpenCode 1 names are refused on OpenCode 2", () => {
      for (const name of ["bash", "apply_patch"]) {
        expect(() =>
          assertV2ToolsAreAft(scenario, offering([{ name, description: "Execute shell commands." }])),
        ).toThrow(`the host offered ${name}`);
      }
    });
  });

  // The pinned V1 host serves a whole scenario and then never ends inside a
  // git project. A row that has already taken that ending out of its verdict
  // does not buy it with its own timeout: once the caller has what it judges,
  // the wait stops. Nothing stops a wait the caller has not finished with.
  describe("a host the caller is finished with is stopped, not waited out", () => {
    async function sleeper(): Promise<{ executable: string; cwd: string }> {
      const parent = await root();
      const executable = await cachedExecutable("#!/bin/sh\nsleep 30\n");
      return { executable, cwd: parent };
    }

    test("the wait ends when the caller says it has everything", async () => {
      const { executable, cwd } = await sleeper();
      const client = startScenarioClient({
        executable,
        scenario: scenario(call()),
        cwd,
        env: { ...process.env },
        processObserver: { trackChild() {} } as never,
        hostGeneration: "v1",
      });

      const startedAt = Date.now();
      const output = await client.wait(20_000, Bun.sleep(200));

      expect(output.stopped_early).toBe(true);
      expect(output.timed_out).toBe(false);
      expect(Date.now() - startedAt).toBeLessThan(10_000);
    });

    test("a wait with nothing to stop it still runs to the timeout", async () => {
      const { executable, cwd } = await sleeper();
      const client = startScenarioClient({
        executable,
        scenario: scenario(call()),
        cwd,
        env: { ...process.env },
        processObserver: { trackChild() {} } as never,
        hostGeneration: "v1",
      });

      const output = await client.wait(700);

      expect(output.timed_out).toBe(true);
      expect(output.stopped_early).toBe(false);
    });
  });

  test("scenario tool aliases use the registered host names", () => {
    expect(hostToolName("ast_search")).toBe("ast_grep_search");
    expect(hostToolName("ast_replace")).toBe("ast_grep_replace");
    expect(hostToolName("read")).toBe("read");
    // OpenCode 1 keeps AFT's names; OpenCode 2 registers bash and apply_patch
    // under its own `shell` and `patch`.
    expect(hostToolName("bash", "v1")).toBe("bash");
    expect(hostToolName("apply_patch", "v1")).toBe("apply_patch");
    expect(hostToolName("bash", "v2")).toBe("shell");
    expect(hostToolName("apply_patch", "v2")).toBe("patch");
    expect(hostToolName("bash_status", "v2")).toBe("bash_status");
    expect(hostToolName("ast_search", "v2")).toBe("ast_grep_search");
    expect(hostToolArguments("read", { filePath: "sample.txt", limit: 10 })).toEqual({
      path: "sample.txt",
      limit: 10,
    });
  });

  test("the mock calls AFT's bash by the name each host generation registered", async () => {
    const turn = {
      label: "call",
      response: {
        kind: "tool_calls" as const,
        calls: [{ id: "c", name: "bash", arguments: { command: "printf x" } }],
      },
    };
    const v1 = await observeThenRespond(structuredClone(turn) as never, {}, 0, {}, "v1");
    const v2 = await observeThenRespond(structuredClone(turn) as never, {}, 0, {}, "v2");
    expect((v1.response.toolCalls as Array<{ name: string }>)[0].name).toBe("bash");
    expect((v2.response.toolCalls as Array<{ name: string }>)[0].name).toBe("shell");
  });

  test("tool result lookup follows the pinned host's 40-character call ids", () => {
    const callId = "callgraph-t6-callgraph-impact-payload-sites-incomplete";
    const exchanges = [
      {
        index: 0,
        label: "turn",
        request: {
          messages: [{ role: "tool", tool_call_id: callId.slice(0, 40), content: "done" }],
        },
        response: {},
        observed_at: "now",
      },
    ];
    expect(toolResultForCall(exchanges, callId)?.text).toBe("done");
  });

  test("title generation is excluded from scripted scenario turns", () => {
    const request = (system: string) => ({
      messages: [
        { role: "system", content: system },
        { role: "user", content: "control" },
      ],
    });
    expect(isTitleGenerationRequest(request("You are a title generator"))).toBe(true);
    expect(isTitleGenerationRequest(request("You are a coding agent"))).toBe(false);
  });

  test("turn-log liveness rejects a missing later turn", () => {
    expect(() => assertTurnLog(["tool", "result", "final"], ["tool", "result"])).toThrow(
      "turn_log_incomplete",
    );
  });

  test("truncation trailers accept lower-bound totals", () => {
    const match = new RegExp(TRUNCATION_TRAILER_PATTERN).exec(
      "shown 5 of ≥140 files (cap) · narrow: path",
    );
    expect(match?.groups).toMatchObject({ shown: "5", total: "140", reason: "cap" });
  });

  test("T6 requires the product-rendered trailer line byte for byte", () => {
    const fixture = scenario(call({ id: "t6", name: "glob", arguments: {} }));
    fixture.id = "glob/T6/incomplete";
    fixture.tool = "glob";
    fixture.trajectory = "T6";
    fixture.metadata = {
      t6: {
        fixture: "incomplete",
        call_id: "t6",
        triggered_reason: "cap",
        expected_trailer: "shown 5 of ≥140 files (cap) · narrow: path",
      },
    };
    expect(() =>
      assertT6Trailer(
        fixture,
        fixture.turns[0].response.kind === "tool_calls"
          ? fixture.turns[0].response.calls[0]
          : call(),
        "files\nshown 5 of ≥140 files (cap) · narrow: path",
      ),
    ).not.toThrow();
    expect(() =>
      assertT6Trailer(
        fixture,
        fixture.turns[0].response.kind === "tool_calls"
          ? fixture.turns[0].response.calls[0]
          : call(),
        "files\nshown 5 of ≥141 files (cap) · narrow: path",
      ),
    ).toThrow("does not exactly equal");
  });

  test("shape projection refuses unparsed agent-visible text", () => {
    expect(() =>
      projectText("count: 2\nunparsed", [
        {
          kind: "field",
          field: "count",
          pattern: "^count: (?<value>\\d+)$",
          types: { value: "number" },
        },
      ]),
    ).toThrow("projection_unparsed:unparsed");
  });
});

describe("shared-server controls", () => {
  test("control path interpolation resolves permission, session, and task ids", async () => {
    const parent = await root();
    const executable = await cachedExecutable(CAPTURE_ARGUMENTS_SCRIPT);
    const argumentsPath = join(parent, "arguments.txt");
    const handoff = { kind: "flag", name: "--server" } as const;
    const contract: HostCliContract = {
      schema_version: 1,
      host_version: "test",
      observed_run_id: "test",
      endpoint_handoff: { run: handoff, api: handoff },
      password_handoff: {
        run: { kind: "env", name: "OPENCODE_SERVER_PASSWORD" },
        api: { kind: "env", name: "OPENCODE_SERVER_PASSWORD" },
      },
      session_start: {},
      idle_retention: {},
      request_body_flag: "--data",
      shared_server_smoke: { method: "GET", path: "/api/health" },
    };

    await runApiControl(
      {
        id: "interpolation-control",
        after_turn: "turn",
        method: "POST",
        path: "/api/session/{{session_id}}/permission/{{permission_id}}/task/{{task_id:source}}",
        purpose: "smoke",
      },
      {
        executable,
        cwd: parent,
        env: { ...process.env, ARGUMENTS_PATH: argumentsPath },
        contract,
        endpoint: "http://127.0.0.1:4096",
        controlPathValues: {
          session_id: "session/one",
          permission_id: "permission two",
          "task_id:source": "bash-three",
        },
      },
    );

    expect((await readFile(argumentsPath, "utf8")).trim().split("\n")).toEqual([
      "api",
      "--server",
      "http://127.0.0.1:4096",
      "POST",
      "/api/session/session%2Fone/permission/permission%20two/task/bash-three",
    ]);
  });

  test("a control body is passed on the flag the captured contract names", async () => {
    const parent = await root();
    const executable = await cachedExecutable(CAPTURE_ARGUMENTS_SCRIPT);
    const argumentsPath = join(parent, "body-arguments.txt");
    const handoff = { kind: "flag", name: "--server" } as const;
    const contract: HostCliContract = {
      schema_version: 1,
      host_version: "test",
      observed_run_id: "test",
      endpoint_handoff: { run: handoff, api: handoff },
      password_handoff: {
        run: { kind: "env", name: "OPENCODE_SERVER_PASSWORD" },
        api: { kind: "env", name: "OPENCODE_SERVER_PASSWORD" },
      },
      session_start: {},
      idle_retention: {},
      request_body_flag: "--data",
      shared_server_smoke: { method: "GET", path: "/api/health" },
    };

    await runApiControl(
      {
        id: "body-control",
        after_turn: "turn",
        method: "POST",
        path: "/api/session/one/permission/two/reply",
        body: { decision: "once" },
        purpose: "permission",
      },
      {
        executable,
        cwd: parent,
        env: { ...process.env, ARGUMENTS_PATH: argumentsPath },
        contract,
        endpoint: "http://127.0.0.1:4096",
      },
    );

    // A body sent on a flag the host does not recognise never reaches it: the
    // command exits 1 before making the request, so the control answers
    // nothing.
    expect((await readFile(argumentsPath, "utf8")).trim().split("\n")).toEqual([
      "api",
      "--server",
      "http://127.0.0.1:4096",
      "POST",
      "/api/session/one/permission/two/reply",
      "--data",
      '{"decision":"once"}',
    ]);
  });

  test("mock hooks observe generated ids before materializing the response", async () => {
    const turn: ScriptedTurn = {
      label: "arm-watch",
      response: {
        kind: "tool_calls",
        calls: [
          {
            id: "watch",
            name: "bash_watch",
            arguments: { taskId: "{{task_id:source}}" },
            non_mutating_evidence: { reason: "the unit fixture only materializes arguments" },
          },
        ],
      },
    };
    const order: string[] = [];
    const { response, exchange } = await observeThenRespond(
      turn,
      { messages: [{ role: "tool", content: "taskId: bash-generated" }] },
      0,
      {
        afterRequest: (exchange, observedTurn) => {
          order.push("observe");
          const taskId = JSON.stringify(exchange.request).match(/\bbash-[A-Za-z0-9_-]+\b/)?.[0];
          if (!observedTurn) throw new Error("scripted turn was not supplied to its request hook");
          materializeTurnPlaceholders(observedTurn, {
            "task_id:source": taskId ?? "",
          });
        },
        afterResponse: () => {
          order.push("respond");
        },
      },
    );

    expect(order).toEqual(["observe", "respond"]);
    expect((response.toolCalls as Array<{ name: string }>)[0].name).toBe("bash_watch");
    expect(JSON.parse(String((response.toolCalls as Array<{ arguments: string }>)[0].arguments))).toEqual(
      { taskId: "bash-generated" },
    );
    expect(exchange.response).toBe(response);
  });

  test("fallback transport revives before the product restore turn", () => {
    const fallback: ScenarioDefinition = {
      schema_version: 1,
      id: "bash/T3/fallback_ask_allow",
      tool: "bash",
      trajectory: "T3",
      execution: "shared-server",
      prompt: "exercise one transient fallback window",
      turns: [
        {
          label: "checkpoint",
          response: {
            kind: "tool_calls",
            calls: [
              {
                id: "checkpoint",
                name: "aft_safety",
                arguments: { op: "checkpoint", name: "fallback" },
              },
            ],
          },
        },
        {
          label: "fallback-call",
          response: {
            kind: "tool_calls",
            calls: [{ id: "fallback", name: "bash", arguments: { command: "printf fallback" } }],
          },
        },
        {
          label: "restore",
          response: {
            kind: "tool_calls",
            calls: [
              {
                id: "restore",
                name: "aft_safety",
                arguments: { op: "restore", name: "fallback" },
              },
            ],
          },
        },
      ],
    };
    const window = resolveTransportDeadWindow(fallback);
    expect(window).toBeDefined();
    expect(
      fallback.turns.map((turn) => transportDeadAtTurn(fallback, window!, turn.label)),
    ).toEqual([false, true, false]);
  });
});

describe("the transport-dead stand-in", () => {
  // A stand-in for the real `aft`: a file that, if a shell ever reads it as a
  // script, leaves a marker in its working directory. The real binary's ELF
  // header did the same thing by accident, through a `>` byte in its first
  // "line". The marker lands in whatever directory the shell runs in, so the
  // file itself can sit in the shared cache.
  function interpretableLiveBinary(): Promise<string> {
    return cachedExecutable("printf interpreted > interpreted-live-binary\n");
  }

  // The swapped `aft` symlink stays in each test's own directory; only the
  // stand-in and its body, which it points at, come from the shared cache.
  // Their content is fixed once the body's path is, so both are reused.
  async function makeStub(base: string) {
    return makeTransportDeadStub(base, await interpretableLiveBinary(), ({ content, executable }) =>
      cachedFile(content, executable),
    );
  }

  test("accepts one request through the swapped path, then dies without answering", async () => {
    const base = await root();
    const stub = await makeStub(base);
    await pointTransportDeadStub(stub, true);
    const child = Bun.spawn([stub.executable], {
      cwd: base,
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
    });
    child.stdin.write('{"id":"1","command":"configure"}\n');
    await child.stdin.end();
    expect(await child.exited).toBe(7);
    expect(await new Response(child.stdout).text()).toBe("");
    expect(await new Response(child.stderr).text()).toBe("");
  });

  test("never reads the real binary as a script when the window closes mid-spawn", async () => {
    const base = await root();
    const project = join(base, "project");
    await mkdir(project);
    const stub = await makeStub(base);
    await pointTransportDeadStub(stub, true);

    // This replays, step by step, what the kernel does when the bridge spawns
    // the stand-in. First it reads the `#!` line of whatever the swapped path
    // names at that instant, to learn which interpreter to start...
    const shebang = (await readFile(stub.executable, "utf8")).split("\n")[0]!;
    expect(shebang.startsWith("#!")).toBe(true);
    const line = shebang.slice(2).trim();
    const split = line.search(/\s/);
    const interpreter = split === -1 ? line : line.slice(0, split);
    const argument = split === -1 ? undefined : line.slice(split).trim();

    // ...the window closes before the interpreter opens anything...
    await pointTransportDeadStub(stub, false);

    // ...and the interpreter starts with the swapped path as its last
    // argument, exactly as Linux passes it.
    const child = Bun.spawn(
      [interpreter, ...(argument ? [argument] : []), stub.executable],
      { cwd: project, stdin: "pipe", stdout: "pipe", stderr: "pipe" },
    );
    child.stdin.write('{"id":"1","command":"configure"}\n');
    await child.stdin.end();
    const exitCode = await child.exited;

    expect(await readdir(project)).toEqual([]);
    expect(exitCode).toBe(7);
  });

  test("refuses a body path that cannot sit in a #! line as one argument", () => {
    expect(() => deadTransportStubSource("relative/body.sh")).toThrow("absolute path");
    expect(() => deadTransportStubSource("/with space/body.sh")).toThrow("without whitespace");
  });
});

describe("source-of-truth derivation", () => {
  test("matrix drift guard rejects missing and removed tool rows by name", () => {
    const cells = Object.fromEntries(
      ["T1", "T2", "T3", "T4", "T5", "T6", "T7"].map((trajectory) => [trajectory, "n/a:test"]),
    ) as never;
    const powershell = Object.fromEntries(
      ["T1", "T2", "T3", "T4", "T5", "T6", "T7"].map((trajectory) => [trajectory, "n/a:platform"]),
    ) as never;
    const projectedControls = ["bash_kill", "bash_status", "bash_watch", "bash_write"];
    const requiredRows = [
      ...projectedControls.map((tool) => ({ tool, trajectories: cells })),
      { tool: "status", trajectories: cells },
      { tool: "powershell", trajectories: powershell },
    ];
    const projection = ["read", ...projectedControls];
    const schemas = {
      read: {},
      powershell: {},
      status: {},
      bash_kill: {},
      bash_status: {},
      bash_write: {},
    };
    expect(() =>
      validateInventory(
        { schema_version: 1, platform: "linux", rows: requiredRows },
        schemas,
        "linux",
        projection,
      ),
    ).toThrow("tool missing matrix row: read");
    expect(() =>
      validateInventory(
        {
          schema_version: 1,
          platform: "linux",
          rows: [
            { tool: "read", trajectories: cells },
            ...requiredRows,
            { tool: "removed", trajectories: cells },
          ],
        },
        schemas,
        "linux",
        projection,
      ),
    ).toThrow("matrix row names removed tool: removed");
  });

  test("V2 projection inventory matches schema through explicit exclusions in both directions", async () => {
    const repo = join(import.meta.dir, "../../../..");
    const scenarios = materializeParityScenarios(
      await loadScenarios(join(repo, "tests", "docker", "opencode2", "scenarios")),
    );
    const projection = deriveV2HarnessProjection(scenarios, "linux");
    const schemas = await loadToolSchemas(repo);
    const cells = Object.fromEntries(
      ["T1", "T2", "T3", "T4", "T5", "T6", "T7"].map((trajectory) => [
        trajectory,
        "n/a:test",
      ]),
    ) as never;
    const powershell = Object.fromEntries(
      ["T1", "T2", "T3", "T4", "T5", "T6", "T7"].map((trajectory) => [
        trajectory,
        "n/a:platform",
      ]),
    ) as never;
    const inventory = [...new Set([...projection, "powershell", "status"])].sort();
    const matrix = {
      schema_version: 1 as const,
      platform: "linux",
      rows: inventory.map((tool) => ({
        tool,
        trajectories: tool === "powershell" ? powershell : cells,
      })),
    };

    expect(validateInventory(matrix, schemas, "linux", projection)).toEqual(inventory);
    const schemaWithProjectedControl = { ...schemas, bash_watch: {} };
    expect(() => validateInventory(matrix, schemaWithProjectedControl, "linux", projection)).toThrow(
      "projection-only tools do not match the explicit exclusion table",
    );
    const { powershell: _powershell, ...schemaWithoutPlatformTool } = schemas;
    expect(() =>
      validateInventory(matrix, schemaWithoutPlatformTool, "linux", projection),
    ).toThrow("schema-only tools do not match the explicit exclusion table");
  });

  test("an observation-only subset validates against the schema projection; a full run reads it as drift", async () => {
    const repo = join(import.meta.dir, "../../../..");
    const scenarios = materializeParityScenarios(
      await loadScenarios(join(repo, "tests", "docker", "opencode2", "scenarios", "read")),
    );
    const subset = await validateHarnessInputs({
      repoRoot: repo,
      scenarios,
      pinnedHostVersion: "0.0.0-beta-test",
      platform: "linux",
      observationOnly: true,
    });
    expect(subset.matrix).toBeDefined();
    expect(subset.inventory).toContain("read");
    expect(subset.inventory).toContain("apply_patch");
    // powershell stays in the inventory as a matrix row; on Linux every one of
    // its cells is n/a:platform and no scenario exists for it.
    const powershell = subset.matrix?.rows.find((row) => row.tool === "powershell");
    expect(powershell?.trajectories.T1).toBe("n/a:platform");

    const error = await expectCode(
      () =>
        validateHarnessInputs({
          repoRoot: repo,
          scenarios,
          pinnedHostVersion: "0.0.0-beta-test",
          platform: "linux",
          fullRun: true,
        }),
      "matrix_invalid",
    );
    // A subset on a full run is read as drift at the first guard it reaches:
    // the scenario-derived projection no longer matches the exclusion table.
    expect(error.message).toContain("do not match the explicit exclusion table");
  });

  test("a callgraph_ready row starts AFT with a warm-up call and gates the real one", () => {
    const input = scenario(call({ name: "aft_callgraph" }));
    input.tool = "callgraph";
    input.preconditions = ["callgraph_ready"];
    const warmed = addCallgraphWarmup(input);
    expect(warmed.expected_turns).toEqual(["turn-1-warmup", "turn-1"]);
    expect(warmed.turns[0].response).toMatchObject({
      calls: [{ id: "warmup-call-1", name: "aft_callgraph" }],
    });
    expect(warmed.turns[0].await_ready).toBeUndefined();
    expect(warmed.turns[1].await_ready).toEqual({
      subject: "callgraph",
      timeout_ms: CALLGRAPH_READY_TIMEOUT_MS,
    });
    // The readiness gate replaces a fixed pause; nothing is left to sleep.
    expect(warmed.turns[1].delay_ms).toBeUndefined();
  });

  test("a callgraph row without the declaration is left as registered", () => {
    const input = scenario(call({ name: "aft_callgraph" }));
    input.tool = "callgraph";
    const unchanged = addCallgraphWarmup(input);
    expect(unchanged).toBe(input);
    expect(unchanged.turns.every((turn) => turn.await_ready === undefined)).toBe(true);
  });

  test("preconditions are validated when registrations load", async () => {
    const directory = await root();
    const registration = (preconditions: unknown, name: string) => ({
      schema_version: 1,
      tool: "callgraph",
      scenarios: [
        {
          ...scenario(call({ name, arguments: {}, id: "call-1" })),
          id: "callgraph/T2/control",
          tool: "callgraph",
          trajectory: "T2",
          preconditions,
        },
      ],
    });
    await writeFile(
      join(directory, "registration.json"),
      JSON.stringify(registration(["index_ready"], "aft_callgraph")),
    );
    const unknown = await expectCode(() => loadScenarios(directory), "scenario_invalid");
    expect(unknown.message).toContain("unknown precondition index_ready");
    await writeFile(
      join(directory, "registration.json"),
      JSON.stringify(registration(["callgraph_ready"], "aft_zoom")),
    );
    const uncalled = await expectCode(() => loadScenarios(directory), "scenario_invalid");
    expect(uncalled.message).toContain("callgraph_ready requires an aft_callgraph call");
  });

  test("every callgraph row AFT answers declares callgraph_ready; host-rejected rows do not", async () => {
    const loaded = await loadScenarios(
      join(import.meta.dir, "..", "scenarios", "aft_callgraph"),
    );
    for (const row of loaded) {
      const gated = row.turns.some((turn) => turn.await_ready?.subject === "callgraph");
      // Host-rejected arguments never reach AFT, so no build would start and
      // a wait could only time out.
      expect({ id: row.id, gated }).toEqual({ id: row.id, gated: row.error_origin !== "host" });
    }
  });

  test("T7 is materialized from the same T1 scenario data", () => {
    const t1: ScenarioDefinition = {
      schema_version: 1,
      id: "read/T1/happy",
      tool: "read",
      trajectory: "T1",
      execution: "standalone",
      prompt: "read",
      turns: [{ label: "done", response: { kind: "text", content: "done" } }],
      comparison: { mode: "exact", expected: "text" },
      compare_call_id: "call",
    };
    const parity = materializeParityScenarios([t1]).find(
      (candidate) => candidate.id === "read/T7/happy",
    );
    expect(parity?.turns).toEqual(t1.turns);
    expect(parity?.comparison).toEqual(t1.comparison);
  });

  test("T6 ownership and fixtures follow negotiated route requirements", () => {
    const surface = {
      id: "future_tool.matches.payload.matches",
      command: "future_tool",
      mode: "matches",
      list_id: "payload.matches",
      owner: "future_tool",
      unit: "results",
      narrow: ["pattern"],
      reasons: ["budget"],
      requires: "future-results/v2",
    };
    const matrix: Parameters<typeof validateT6>[0] = {
      schema_version: 1,
      platform: "linux",
      rows: [{
        tool: surface.owner,
        trajectories: {
          T1: "applicable", T2: "applicable", T3: "applicable", T4: "applicable",
          T5: "applicable", T6: "n/a:no-list-surface", T7: "applicable",
        },
      }],
    };
    expect(() => validateT6(matrix, [], [surface])).not.toThrow();
    expect(() => validateT6(matrix, [], [surface], ["unrelated/v1"])).not.toThrow();
    expect(() => validateT6(matrix, [], [surface], [surface.requires]))
      .toThrow("owns registered list surfaces");
    matrix.rows[0].trajectories.T6 = "applicable";
    const fixtures: ScenarioDefinition[] = ["complete", "incomplete"].map((kind) => ({
      ...scenario(call({ id: "future-call", name: surface.owner })),
      id: `${surface.owner}/T6/${kind}`,
      tool: surface.owner,
      trajectory: "T6",
      metadata: {
        t6: {
          surface_id: surface.id,
          call_id: "future-call",
          fixture: kind,
          ...(kind === "incomplete" ? {
            triggered_reason: "budget",
            expected_trailer: "shown 1 of 2 results (budget) · narrow: pattern",
          } : {}),
        },
      },
    }));
    expect(() => validateT6(matrix, fixtures, [surface], [surface.requires])).not.toThrow();
    expect(() => validateT6(matrix, fixtures.slice(0, 1), [surface], [surface.requires]))
      .toThrow("requires complete and incomplete fixtures");
    expect(() => validateT6(matrix, fixtures, [surface])).toThrow("must derive n/a:no-list-surface");
    matrix.rows[0].trajectories.T6 = "n/a:no-list-surface";
    expect(() => validateT6(matrix, fixtures, [surface])).toThrow("unknown T6 surface");
  });

  test("all fifteen list surfaces and route requirements come from the Rust registry", async () => {
    const repo = join(import.meta.dir, "../../../..");
    const surfaces = await deriveListSurfaces(repo);
    expect(surfaces).toHaveLength(15);
    expect(surfaces.find((surface) => surface.id === "ast_search.matches.payload.matches"))
      .toMatchObject({
        owner: "ast_search",
        requires: "tool-result/v1",
        unit: "results",
        reasons: ["budget"],
        narrow: ["paths", "globs", "pattern"],
      });
    expect(surfaces.filter((surface) => surface.requires == null)).toHaveLength(14);
    expect(surfaces.find((surface) => surface.id === "bash.changed_files.bash.changed_files"))
      .toMatchObject({ owner: "bash", unit: "files" });
    expect(surfaces.find((surface) => surface.id === "bash.diff_lines.bash.diff_lines"))
      .toMatchObject({ owner: "bash", unit: "lines" });
    expect(surfaces.find((surface) => surface.id === "read.directory.payload.entries")).toMatchObject({
      owner: "read",
      unit: "items",
      reasons: ["walk", "cap"],
      narrow: ["path", "offset", "limit"],
    });
    expect(
      surfaces.find((surface) => surface.id === "callgraph.trace_data.payload.hops")?.reasons,
    ).toEqual(["depth"]);
    expect(
      surfaces.find((surface) => surface.id === "outline.files.payload.files")?.reasons,
    ).toEqual(["budget", "walk"]);
    expect(surfaces.find((surface) => surface.id === "grep..payload.matches")).toMatchObject({
      unit: "rows",
      narrow: ["offset", "path", "include"],
      reasons: ["walk", "cap", "kind"],
    });
    const matrix = JSON.parse(
      await readFile(join(repo, "tests/docker/opencode2/matrix/applicability.json"), "utf8"),
    );
    const scenarios = materializeParityScenarios(
      await loadScenarios(join(repo, "tests/docker/opencode2/scenarios")),
    );
    expect(() => validateT6(matrix, scenarios, surfaces)).not.toThrow();
    const incompatibleGlob = scenarios.find(
      (candidate) => candidate.id === "glob/T6/glob-payload-files/incomplete",
    );
    if (!incompatibleGlob) throw new Error("incomplete glob fixture missing");
    incompatibleGlob.metadata = structuredClone(incompatibleGlob.metadata);
    (incompatibleGlob.metadata?.t6 as Record<string, unknown>).expected_trailer =
      "shown 5 of ≥140 files (cap) · narrow: include";
    expect(() => validateT6(matrix, scenarios, surfaces)).toThrow(
      "expected trailer disagrees with the registry",
    );
  });

  test("mutating tools derive from product permission metadata in both directions", async () => {
    const repo = join(import.meta.dir, "../../../..");
    expect([...(await deriveMutatingTools(repo))].sort()).toEqual([
      "apply_patch",
      "ast_replace",
      "bash",
      "delete",
      "edit",
      "import",
      "move",
      "safety",
      "write",
    ]);
  });

  test("permission operations come from the exact exported inventory", async () => {
    const repo = join(import.meta.dir, "../../../..");
    expect(await readPermissionAskInventory(repo)).toEqual([
      "read",
      "edit",
      "write",
      "apply_patch",
      "aft_delete",
      "aft_move",
      "bash:withPermissionLoop",
      "bash:host-fallback",
    ]);
  });
});

describe("background liveness clocks", () => {
  test("completion wake requires one host-transcript steer request", () => {
    const completionScenario = scenario(call());
    completionScenario.id = "bash/T5/completion_wake";
    completionScenario.tool = "bash";
    completionScenario.trajectory = "T5";
    completionScenario.execution = "shared-server";
    completionScenario.metadata = {
      t5: { source_call_id: "source", wake_turn: "wake-observed" },
    };
    const exchange = {
      index: 0,
      label: "wake-observed",
      request: {
        messages: [
          {
            role: "user",
            content: "<system-reminder>\n[BACKGROUND BASH COMPLETED]\n- task bash-control\n</system-reminder>",
          },
        ],
      },
      response: {},
      observed_at: "now",
    };
    expect(() =>
      assertT5HostWakeTranscript(completionScenario, [exchange], {
        "task_id:source": "bash-control",
      }),
    ).not.toThrow();
    expect(() =>
      assertT5HostWakeTranscript(completionScenario, [], {
        "task_id:source": "bash-control",
      }),
    ).toThrow("host transcript contains 0 completion steer wakes");
  });

  test("watch setup, stdout antecedent, delivery, and duplicate window pass", () => {
    const clock = new WatchPatternLiveness({
      scenario: "bash/T5/watch_pattern_once",
      pattern: "READY",
      duplicateWindowMs: 100,
    });
    clock.accepted(0);
    clock.watchArmed(1);
    clock.taskStdout("READY", 2);
    clock.taskStdout("READY", 3);
    clock.delivery(4);
    expect(() => clock.assertComplete(104)).not.toThrow();
  });

  test("watch fixture with no first match fails its setup clock", () => {
    const clock = new WatchPatternLiveness({
      scenario: "bash/T5/watch_pattern_once",
      pattern: "READY",
      duplicateWindowMs: 100,
    });
    clock.accepted(0);
    clock.watchArmed(1);
    expect(() => clock.assertAt(30_000)).toThrow("setup_timeout:first_match");
  });

  test("a second completion cannot re-inject the first prompt", () => {
    const clock = new CompletionWakeLiveness({
      scenario: "bash/T5/completion_wake",
      rearmWindowMs: 100,
    });
    clock.taskStarted("first", 0);
    clock.promptInjected("first", "prompt-1", 1);
    clock.taskStarted("second", 2);
    expect(() => clock.promptInjected("second", "prompt-1", 3)).toThrow("duplicate_delivery");
  });
});

describe("tagged disk states and restore", () => {
  test("three-state create, delete, edit, and move transitions are non-vacuous", () => {
    const absent: PathState = { state: "absent" };
    const a: PathState = { state: "present", sha256: "a" };
    const b: PathState = { state: "present", sha256: "b" };
    const before = { create: absent, delete: a, edit: a, source: a, destination: absent };
    const intermediate = { create: b, delete: absent, edit: b, source: absent, destination: a };
    const restored = { ...before };
    expect(() =>
      assertThreeStateRestore(before, intermediate, restored, [
        { path: "create", transition: "create", expected_intermediate_sha256: "b" },
        { path: "delete", transition: "delete" },
        { path: "edit", transition: "edit" },
        { path: "source", transition: "move_source" },
        { path: "destination", transition: "move_destination", paired_path: "source" },
      ]),
    ).not.toThrow();
  });

  test("skip-mutation control fails at the intermediate checkpoint", () => {
    const present: PathState = { state: "present", sha256: "same" };
    expect(() =>
      assertThreeStateRestore({ file: present }, { file: present }, { file: present }, [
        { path: "file", transition: "edit" },
      ]),
    ).toThrow("no_effect_observed:file");
  });

  test("absent paths are tagged instead of hashed", async () => {
    const project = await root();
    expect(await snapshotPaths(project, ["missing.txt"])).toEqual({
      "missing.txt": { state: "absent" },
    });
  });
});

describe("whole-root asynchronous observation controls", () => {
  test("concurrent-scenario-root-isolation", async () => {
    const firstRoot = await root();
    const secondRoot = await root();
    const first = new DiskStateObserver(firstRoot, "write/T1/concurrent-first");
    const second = new DiskStateObserver(secondRoot, "write/T1/concurrent-second");
    await Promise.all([
      first.beginCall(call({ disk_effects: ["effect.txt"] })),
      second.beginCall(call({ disk_effects: ["effect.txt"] })),
    ]);
    await Promise.all([
      writeFile(join(firstRoot, "effect.txt"), "first scenario\n"),
      writeFile(join(secondRoot, "effect.txt"), "second scenario\n"),
    ]);
    await Promise.all([
      first.checkpointCall("call-1", "tool-result", "result"),
      second.checkpointCall("call-1", "tool-result", "result"),
    ]);
    expect(first.failures).toEqual([]);
    expect(second.failures).toEqual([]);
  });

  // Concurrent scenarios each own a root and an observer. A write that lands
  // in another scenario's root is reported by THAT root's observer under the
  // victim's scenario id: per-root observation cannot know the writer, only
  // where the effect landed, and two observers must never blur.
  test("cross-scenario-root-write-attributed-to-victim-root", async () => {
    const writerRoot = await root();
    const victimRoot = await root();
    const writer = new DiskStateObserver(writerRoot, "bash/T1/cross-root-writer");
    const victim = new DiskStateObserver(victimRoot, "read/T1/cross-root-victim");
    await Promise.all([writer.beginCall(call({ name: "bash" })), victim.beginCall(call())]);

    await writeFile(join(victimRoot, "escaped.txt"), "cross-root effect\n");
    await writer.checkpointCall("call-1", "tool-result", "result");
    const error = await expectCode(
      () => victim.checkpointCall("call-1", "tool-result", "result"),
      "undeclared_disk_effect",
    );
    expect(writer.failures).toEqual([]);
    expect(error.details).toMatchObject({
      scenario: "read/T1/cross-root-victim",
      originating_call: "call-1",
      path: "escaped.txt",
    });
  });

  test("ordinary-undeclared-disk-effect", async () => {
    const project = await root();
    const observer = new DiskStateObserver(project, "write/T1/ordinary-control");
    await observer.beginCall(call());
    await writeFile(join(project, "changed.txt"), "undeclared\n");
    const error = await expectCode(
      () => observer.checkpointCall("call-1", "tool-result", "result"),
      "undeclared_disk_effect",
    );
    expect(error.details.path).toBe("changed.txt");
  });

  test("test-override-undeclared-disk-effect", async () => {
    const override = applyMutatingTestOverride(
      new Set(["write"]),
      {
        AFT_OPENCODE2_HARNESS_SELF_TEST: "1",
        AFT_OPENCODE2_TEST_REMOVE_MUTATING_TOOL: "write",
        AFT_OPENCODE2_TEST_NON_MUTATING_EVIDENCE: "fabricated control evidence",
      },
      true,
    );
    validateMutatingDeclarations([scenario(call())], override.tools, override.fabricatedEvidence);

    const project = await root();
    const observer = new DiskStateObserver(project, "write/T1/override-control");
    await observer.beginCall(call());
    await writeFile(join(project, "changed.txt"), "still observed\n");
    await expectCode(
      () => observer.checkpointCall("call-1", "tool-result", "result"),
      "undeclared_disk_effect",
    );
  });

  test("post-result-undeclared-disk-effect", async () => {
    const project = await root();
    const observer = new DiskStateObserver(project, "bash/T5/post-result-control");
    await observer.beginCall(call({ name: "bash", outlives_result: true }));
    await observer.checkpointCall("call-1", "initial-result", "result");
    await writeFile(join(project, "late.txt"), "after result\n");
    const error = await expectCode(
      () => observer.checkpointCall("call-1", "task-terminal", "post_result"),
      "undeclared_disk_effect",
    );
    expect(error.details.phase).toBe("post_result");
  });

  test("declared-post-result-effect", async () => {
    const project = await root();
    const observer = new DiskStateObserver(project, "bash/T5/declared-post-result");
    await observer.beginCall(
      call({
        name: "bash",
        outlives_result: true,
        disk_effects: [{ path: "late.txt", phase: "post_result" }],
      }),
    );
    await observer.checkpointCall("call-1", "initial-result", "result");
    await writeFile(join(project, "late.txt"), "after result\n");
    await observer.checkpointCall("call-1", "task-terminal", "post_result");
    observer.markTerminal("call-1");
    await observer.finalize(true);
  });

  test("cancellation-undeclared-disk-effect", async () => {
    const project = await root();
    const observer = new DiskStateObserver(project, "bash/T4/cancellation-control");
    await observer.beginCall(call({ name: "bash", outlives_result: true }));
    await observer.checkpointCall("call-1", "initial-result", "result");
    await writeFile(join(project, "cancelled.txt"), "before stop\n");
    const error = await expectCode(
      () => observer.checkpointCall("call-1", "cancel-confirmed", "cancellation"),
      "undeclared_disk_effect",
    );
    expect(error.details.phase).toBe("cancellation");
  });

  test("surviving-writer-incomplete", async () => {
    const project = await root();
    const observer = new DiskStateObserver(project, "bash/T5/surviving-writer");
    await observer.beginCall(call({ name: "bash", outlives_result: true }));
    await observer.checkpointCall("call-1", "initial-result", "result");
    await expectCode(() => observer.finalize(false), "disk_effect_observation_incomplete");
  });
});

describe("producer-backed executable provenance", () => {
  async function fixture(): Promise<{
    repo: string;
    executable: string;
    policy: string;
    manifest: string;
    sha: string;
  }> {
    const repo = await root();
    Bun.spawnSync(["git", "init", "-q"], { cwd: repo });
    await writeFile(join(repo, "tracked"), "content\n");
    Bun.spawnSync(["git", "add", "tracked"], { cwd: repo });
    Bun.spawnSync(
      [
        "git",
        "-c",
        "user.name=Harness",
        "-c",
        "user.email=harness@example.invalid",
        "commit",
        "-qm",
        "fixture",
      ],
      { cwd: repo },
    );
    const sha = Bun.spawnSync(["git", "rev-parse", "HEAD"], { cwd: repo }).stdout.toString().trim();
    const executable = join(repo, "aft");
    // The provenance check looks for the sidecar next to the path it is given
    // and never resolves symlinks, so a symlink in the fixture repository can
    // stand for a shared cached script. The script reads the SHA from the
    // repository it runs in (the check launches it there) rather than having a
    // per-run SHA baked in, so its content, and therefore its path, is fixed.
    // Tests below only rewrite the sidecar, never the executable, so nothing
    // writes through the symlink into the shared copy.
    await symlink(
      await cachedExecutable('#!/bin/sh\necho "aft test ($(git rev-parse HEAD))"\n'),
      executable,
    );
    await writeFile(
      join(repo, "build-info.json"),
      `${JSON.stringify({
        git_sha: sha,
        build_profile: "release",
        sha256: await sha256File(executable),
        source: "checkout-build",
      })}\n`,
    );
    const policy = join(repo, "policy.json");
    await writeFile(
      policy,
      `${JSON.stringify({
        schema_version: 1,
        sidecar_name: "build-info.json",
        allowed_sources: ["checkout-build", "same-sha-artifact"],
        allowed_profiles: ["release"],
        required_sidecar_fields: ["git_sha", "build_profile", "sha256", "source"],
      })}\n`,
    );
    return { repo, executable, policy, manifest: join(repo, "run", "aft_binary.json"), sha };
  }

  test("verified producer sidecar is copied into the run manifest", async () => {
    const item = await fixture();
    const manifest = await verifyExecutableProvenance({
      executable: item.executable,
      repoRoot: item.repo,
      policyPath: item.policy,
      manifestPath: item.manifest,
    });
    expect(manifest.git_sha).toBe(item.sha);
    expect(manifest.source).toBe("checkout-build");
    expect(manifest.verdict).toBe("verified");
  });

  test("missing producer source is unsuppressibly rejected", async () => {
    const item = await fixture();
    const sidecar = JSON.parse(await Bun.file(join(item.repo, "build-info.json")).text());
    delete sidecar.source;
    await writeFile(join(item.repo, "build-info.json"), JSON.stringify(sidecar));
    const error = await expectCode(
      () =>
        verifyExecutableProvenance({
          executable: item.executable,
          repoRoot: item.repo,
          policyPath: item.policy,
          manifestPath: item.manifest,
        }),
      "executable_provenance",
    );
    expect(error.unsuppressible).toBe(true);
  });
});

test("provider contract supplies the observed run model and config key to the harness", async () => {
  const contractRoot = await root();
  await writeFile(
    join(contractRoot, "host-provider-config.json"),
    JSON.stringify({
      schema_version: 1,
      host_version: "0.0.0-beta-test",
      observed_run_id: "probe-run",
      provider_config: { openai: {} },
      opencode_json: { providers: { openai: {} } },
      run_command: ["opencode2", "run", "--model", "openai/mock-model", "message"],
    }),
  );

  const contract = await loadHostProviderConfigContract(contractRoot, "0.0.0-beta-test");
  expect(contract.model).toBe("openai/mock-model");
  expect(contract.config_key).toBe("providers");
});

test("a provider contract that does not show its config key is rejected", async () => {
  const contractRoot = await root();
  await writeFile(
    join(contractRoot, "host-provider-config.json"),
    JSON.stringify({
      schema_version: 1,
      host_version: "0.0.0-beta-test",
      observed_run_id: "probe-run",
      provider_config: { openai: {} },
      run_command: ["opencode2", "run", "--model", "openai/mock-model", "message"],
    }),
  );
  await expectCode(
    () => loadHostProviderConfigContract(contractRoot, "0.0.0-beta-test"),
    "contract_uncaptured",
  );
});

test("the V1 leg reads its own provider contract, under its own key", async () => {
  const contractRoot = await root();
  await writeFile(
    join(contractRoot, "host1-provider-config.json"),
    JSON.stringify({
      schema_version: 1,
      host_version: "1.18.30",
      observed_run_id: "probe-run",
      provider_config: { mock: { npm: "@ai-sdk/openai-compatible" } },
      opencode_json: { provider: { mock: { npm: "@ai-sdk/openai-compatible" } } },
      run_command: ["opencode", "run", "--model", "mock/mock-model", "message"],
    }),
  );

  const contract = await loadV1HostProviderConfigContract(contractRoot, "1.18.30");
  expect(contract.model).toBe("mock/mock-model");
  expect(contract.config_key).toBe("provider");
});

test("legacy host CLI capture names every field required by the runner", async () => {
  const contractRoot = await root();
  await writeFile(
    join(contractRoot, "host-cli-contract.json"),
    JSON.stringify({
      schema_version: 1,
      host_version: "0.0.0-beta-test",
      observed_run_id: "probe-run",
      endpoint_handoff: { run_client_arguments: ["--server", "${endpoint}"] },
      password_handoff: { observed_run_client_environment: "OPENCODE_SERVER_PASSWORD" },
      session_start: {},
      idle_retention: {},
      shared_server_smoke: { positive_controls: [] },
    }),
  );
  const error = await expectCode(
    () => loadHostCliContract(contractRoot, "0.0.0-beta-test"),
    "contract_uncaptured",
  );
  expect(error.details.missing_fields).toEqual([
    "endpoint_handoff.run",
    "endpoint_handoff.api",
    "password_handoff.run",
    "password_handoff.api",
    "request_body_handoff.api",
    "shared_server_smoke.method",
    "shared_server_smoke.path",
  ]);
});

test("transcript-shaped host schema capture satisfies validation", async () => {
  const contractRoot = await root();
  await writeFile(
    join(contractRoot, "host-schema-rejection.json"),
    JSON.stringify({
      schema_version: 1,
      host_version: "0.0.0-beta-test",
      observed_run_id: "probe-run",
      observed_instance: { json_event_error: "Invalid arguments for tool read" },
      agent_visible_contract: { json_event: { type: "tool_use", state_status: "error" } },
    }),
  );
  const contract = await loadHostSchemaRejectionContract(
    contractRoot,
    "0.0.0-beta-test",
  );
  expect(contract.agent_visible_text).toBe("Invalid arguments for tool read");
  expect(contract.json_event).toEqual({ type: "tool_use", state_status: "error" });
});

test("missing host schema rejection contract fails validation instead of earning coverage", async () => {
  const contractRoot = await root();
  await expectCode(
    () => loadHostSchemaRejectionContract(contractRoot, "0.0.0-beta-test"),
    "contract_uncaptured",
  );
});

describe("permission scenarios reach the host's own rules", () => {
  const permissionScenario = (
    id: string,
    tool: string,
    permission: Record<string, unknown>,
    calls: ToolCallPlan[],
  ): ScenarioDefinition => ({
    schema_version: 1,
    id,
    tool,
    trajectory: "T3",
    execution: "shared-server",
    prompt: "permission",
    turns: [{ label: "call-tool", response: { kind: "tool_calls", calls } }],
    metadata: { permission },
  });

  test("an ask scenario installs one ask rule for the operation it gates", () => {
    const rules = sessionPermissionRules(
      permissionScenario(
        "apply_patch/T3/apply_patch_ask_allow",
        "apply_patch",
        { operation: "apply_patch", reply: "once", requested_paths: ["patched.txt"] },
        [call({ id: "mutate", name: "apply_patch", arguments: {} })],
      ),
    );

    // The resource stays "*" because the tools disagree about how they name
    // one: a rule naming "patched.txt" matches the edit tool's relative path
    // but misses aft_delete's absolute path and bash's command line.
    expect(rules).toEqual([{ action: "edit", resource: "*", effect: "ask" }]);
  });

  test("every mutating operation asks under edit; read under its own name, bash under shell", () => {
    expect(permissionAction("apply_patch")).toBe("edit");
    expect(permissionAction("write")).toBe("edit");
    expect(permissionAction("aft_move")).toBe("edit");
    expect(permissionAction("read")).toBe("read");
    expect(permissionAction("bash:withPermissionLoop")).toBe("shell");
    expect(permissionAction("bash:host-fallback")).toBe("shell");
  });

  test("a configured denial installs a deny rule and leaves nothing to answer", () => {
    const denial = permissionScenario(
      "read/T3/read_config_deny",
      "read",
      { operation: "read", reply: "config_deny", requested_path: "sample.txt" },
      [call({ id: "deny-call", name: "read", arguments: { filePath: "sample.txt" } })],
    );

    expect(sessionPermissionRules(denial)).toEqual([
      { action: "read", resource: "*", effect: "deny" },
    ]);
    expect(controlPlans(denial)).toEqual([]);
  });

  test("a scenario with no permission metadata installs no rules", () => {
    expect(sessionPermissionRules(scenario(call()))).toBeUndefined();
  });

  test("the approved and the rejected prompt are both read off the host's event stream", () => {
    const approved = permissionScenario(
      "apply_patch/T3/apply_patch_ask_allow",
      "apply_patch",
      { operation: "apply_patch", reply: "once", requested_paths: ["patched.txt"] },
      [call({ id: "mutate", name: "apply_patch", arguments: {} })],
    );
    const rejected = permissionScenario(
      "read/T3/read_ask_deny",
      "read",
      { operation: "read", reply: "reject", requested_path: "sample.txt" },
      [call({ id: "read-ask_deny", name: "read", arguments: { filePath: "sample.txt" } })],
    );
    const asked = (id: string, action: string, sourceId: string): HostEvent => ({
      type: "permission.asked",
      data: { id, action, resources: ["whatever the tool calls it"], source: { id: sourceId } },
    });
    const replied = (requestID: string, reply: string): HostEvent => ({
      type: "permission.replied",
      data: { requestID, reply },
    });

    assertPermissionPromptObserved(approved, [
      asked("per_1", "edit", "mutate"),
      replied("per_1", "once"),
    ]);
    assertPermissionPromptObserved(rejected, [
      asked("per_2", "read", "read-ask_deny"),
      replied("per_2", "reject"),
    ]);

    // A tool that completed without asking is the defect these rows exist to
    // catch, and it leaves no event behind.
    expect(() => assertPermissionPromptObserved(approved, [])).toThrow("no_effect_observed");
    // A prompt raised by some other call is not this row's prompt.
    expect(() =>
      assertPermissionPromptObserved(approved, [
        asked("per_3", "edit", "some-other-call"),
        replied("per_3", "once"),
      ]),
    ).toThrow("no_effect_observed");
    // Raised but answered the other way round is a different outcome than the
    // row declares.
    expect(() =>
      assertPermissionPromptObserved(approved, [
        asked("per_4", "edit", "mutate"),
        replied("per_4", "reject"),
      ]),
    ).toThrow("no_effect_observed");
  });

  test("a configured denial is expected to raise no prompt at all", () => {
    const denial = permissionScenario(
      "read/T3/read_config_deny",
      "read",
      { operation: "read", reply: "config_deny", requested_path: "sample.txt" },
      [call({ id: "deny-call", name: "read", arguments: { filePath: "sample.txt" } })],
    );

    assertPermissionPromptObserved(denial, []);
  });

  test("a permission scenario adds no control of its own", () => {
    expect(
      controlPlans(
        permissionScenario(
          "read/T3/read_ask_deny",
          "read",
          { operation: "read", reply: "reject", requested_path: "sample.txt" },
          [call({ id: "read-ask_deny", name: "read", arguments: { filePath: "sample.txt" } })],
        ),
      ),
    ).toEqual([]);
  });

  test("the shared server registers as a service and hands over that password", async () => {
    const parent = await root();
    const stateRoot = join(parent, "xdg-state");
    const argumentsPath = join(parent, "serve-arguments.txt");
    const executable = await cachedExecutable(
      `#!/bin/sh\n` +
        `printf '%s\\n' "$@" > "$ARGUMENTS_PATH"\n` +
        `mkdir -p "$XDG_STATE_HOME/opencode"\n` +
        `printf '%s' '{"id":"x","version":"2.0.11","url":"http://127.0.0.1:4242",` +
        `"pid":1,"password":"from-registration"}' > "$XDG_STATE_HOME/opencode/service.json"\n` +
        `echo "server listening on http://127.0.0.1:4242"\n` +
        `sleep 30\n`,
    );

    const server = await startSharedServer({
      executable,
      cwd: parent,
      env: { ...process.env, XDG_STATE_HOME: stateRoot, ARGUMENTS_PATH: argumentsPath },
      processObserver: new ProcessObserver("registration-test"),
      stateRoot,
      timeoutMs: 10_000,
    });

    try {
      // `--service` is the flag that makes the host publish the registration
      // file `discover()` reads; a plain `serve` is invisible to the plugin
      // running inside it.
      expect((await readFile(argumentsPath, "utf8")).trim().split("\n")).toContain("--service");
      expect(server.endpoint).toBe("http://127.0.0.1:4242");
      expect(server.password).toBe("from-registration");
      expect(server.registration.pid).toBe(1);
    } finally {
      server.child.kill("SIGKILL");
    }
  });

  test("a server that listens without registering is rejected", async () => {
    const parent = await root();
    const executable = await cachedExecutable(
      '#!/bin/sh\necho "server listening on http://127.0.0.1:4242"\nsleep 30\n',
    );

    await expectCode(
      () =>
        startSharedServer({
          executable,
          cwd: parent,
          env: { ...process.env, XDG_STATE_HOME: join(parent, "xdg-state") },
          processObserver: new ProcessObserver("unregistered-test"),
          stateRoot: join(parent, "xdg-state"),
          timeoutMs: 2_000,
        }),
      "host_failed",
    );
  });
});

describe("the artifact store keeps the newest run roots and nothing older", () => {
  const HOUR = 60 * 60 * 1000;

  async function runRoot(parent: string, name: string, ageMs: number, now: number) {
    const path = join(parent, name);
    await mkdir(join(path, "scenarios"), { recursive: true });
    const at = new Date(now - ageMs);
    await utimes(join(path, "scenarios"), at, at);
    await utimes(path, at, at);
    return path;
  }

  test("older roots go, the newest ones and this run's own stay", async () => {
    const parent = await root();
    const now = Date.now();
    const current = await runRoot(parent, "run-current", 0, now);
    await runRoot(parent, "run-newest", 2 * HOUR, now);
    await runRoot(parent, "run-middle", 5 * HOUR, now);
    const oldest = await runRoot(parent, "run-oldest", 9 * HOUR, now);

    const removed = await pruneOldRunRoots({ parent, keep: 3, current, now });

    expect(removed).toEqual([oldest]);
    expect((await readdir(parent)).sort()).toEqual(["run-current", "run-middle", "run-newest"]);
  });

  // A run writes into its subdirectories, not into its root, so the root's own
  // timestamp stops moving early. Reading the children is what keeps a
  // concurrent run's evidence from being deleted underneath it.
  test("a root something is still writing to is left alone", async () => {
    const parent = await root();
    const now = Date.now();
    const current = await runRoot(parent, "run-current", 0, now);
    const stale = await runRoot(parent, "run-elderly-root", 9 * HOUR, now);
    const busy = new Date(now - 60 * 1000);
    await utimes(join(stale, "scenarios"), busy, busy);
    await runRoot(parent, "run-abandoned", 9 * HOUR, now);

    const removed = await pruneOldRunRoots({ parent, keep: 1, current, now });

    expect(removed).toEqual([join(parent, "run-abandoned")]);
    expect((await readdir(parent)).sort()).toEqual(["run-current", "run-elderly-root"]);
  });

  // A run that just finished holds a place in the quota even though it is too
  // recent to remove, so the roots that go are the oldest ones rather than
  // whatever happens to be over the line.
  test("a recent run counts against the quota it is too young to be removed for", async () => {
    const parent = await root();
    const now = Date.now();
    const current = await runRoot(parent, "run-current", 0, now);
    await runRoot(parent, "run-minutes-ago", 10 * 60 * 1000, now);
    await runRoot(parent, "run-older", 5 * HOUR, now);
    await runRoot(parent, "run-oldest", 9 * HOUR, now);

    const removed = await pruneOldRunRoots({ parent, keep: 2, current, now });

    expect(removed.map((path) => path.split("/").at(-1)).sort()).toEqual([
      "run-older",
      "run-oldest",
    ]);
    expect((await readdir(parent)).sort()).toEqual(["run-current", "run-minutes-ago"]);
  });

  test("retention is a positive count, and three by default", () => {
    expect(parseArtifactRetention(undefined)).toBe(3);
    expect(parseArtifactRetention("1")).toBe(1);
    expect(() => parseArtifactRetention("0")).toThrow("AFT_E2E_ARTIFACT_RETAIN");
  });
});

describe("a killed process group is not a running writer", () => {
  async function procRoot(entries: Array<{ pid: number; state: string; pgid: number }>) {
    const directory = await root();
    for (const entry of entries) {
      await mkdir(join(directory, String(entry.pid)), { recursive: true });
      await writeFile(
        join(directory, String(entry.pid), "stat"),
        `${entry.pid} (sleep 600) ${entry.state} 1 ${entry.pgid} 0 0 -1 4194304\n`,
      );
    }
    return directory;
  }

  test("a group whose members have all exited counts as stopped", async () => {
    const directory = await procRoot([{ pid: 345, state: "Z", pgid: 345 }]);

    expect(processGroupRunning(345, directory)).toBe(false);
  });

  test("one member still scheduled keeps the whole group running", async () => {
    const directory = await procRoot([
      { pid: 345, state: "Z", pgid: 345 },
      { pid: 346, state: "S", pgid: 345 },
    ]);

    expect(processGroupRunning(345, directory)).toBe(true);
  });

  test("a group with no listable member is left as running", async () => {
    const directory = await procRoot([{ pid: 999, state: "Z", pgid: 999 }]);

    expect(processGroupRunning(345, directory)).toBe(true);
  });

  test("a system that does not publish process state gives no answer", () => {
    expect(processGroupRunning(345, join(tmpdir(), "opencode2-absent-proc"))).toBeUndefined();
  });
});

describe("a row may take one named thing out of its own verdict", () => {
  const UPSTREAM = "https://github.com/anomalyco/opencode/issues/48340";
  const HOST_SOURCE = "contract/host1-edit-family-gate.json";

  const exclusion = (overrides: Record<string, unknown> = {}) => ({
    subject: "v1_host_process_exit",
    issue: UPSTREAM,
    reason: "The V1 host serves the scenario and then never exits in a git project.",
    ...overrides,
  });

  async function matrixRoot(row: Record<string, unknown>): Promise<string> {
    const directory = await root();
    await writeFile(
      join(directory, "applicability.json"),
      JSON.stringify({
        schema_version: 1,
        platform: "linux",
        rows: [
          {
            trajectories: Object.fromEntries(
              ["T1", "T2", "T3", "T4", "T5", "T6", "T7"].map((trajectory) => [
                trajectory,
                "applicable",
              ]),
            ),
            ...row,
          },
        ],
      }),
    );
    return directory;
  }

  test("the row states what is excluded, the issue it rests on, and why", async () => {
    const matrix = await loadApplicabilityMatrix(
      await matrixRoot({ tool: "bash", verdict_exclusions: { T7: exclusion() } }),
    );

    expect(matrix?.rows[0].verdict_exclusions?.T7).toEqual({
      subject: "v1_host_process_exit",
      issue: UPSTREAM,
      reason: "The V1 host serves the scenario and then never exits in a git project.",
    });
  });

  test("a row cannot exclude something the harness does not know how to leave out", async () => {
    const directory = await matrixRoot({
      tool: "bash",
      verdict_exclusions: { T7: exclusion({ subject: "slow_rows" }) },
    });

    await expect(loadApplicabilityMatrix(directory)).rejects.toThrow(
      "the harness excludes nothing called slow_rows",
    );
  });

  test("an exclusion is refused on a trajectory that never applies it", async () => {
    const directory = await matrixRoot({ tool: "bash", verdict_exclusions: { T1: exclusion() } });

    await expect(loadApplicabilityMatrix(directory)).rejects.toThrow(
      "v1_host_process_exit is only excluded on T7",
    );
  });

  test("an exclusion without the upstream issue behind it is refused", async () => {
    const directory = await matrixRoot({
      tool: "bash",
      verdict_exclusions: { T7: exclusion({ issue: "https://github.com/anomalyco/opencode" }) },
    });

    await expect(loadApplicabilityMatrix(directory)).rejects.toThrow(
      "a verdict exclusion needs the upstream issue it rests on",
    );
  });

  // A subject about deliberate host behaviour has no report to follow, so it
  // carries the captured reading of the host's own code instead. Naming an
  // issue for it would invent a defect that nobody filed.
  test("a subject that rests on host source is refused when it cites an issue instead", async () => {
    const directory = await matrixRoot({
      tool: "apply_patch",
      verdict_exclusions: {
        T7: { subject: "v1_host_edit_family_gate", issue: UPSTREAM, reason: "The host gates it." },
      },
    });

    await expect(loadApplicabilityMatrix(directory)).rejects.toThrow(
      "v1_host_edit_family_gate rests on host source, not an upstream issue",
    );
  });

  test("a subject that rests on host source is refused when it cites nothing", async () => {
    const directory = await matrixRoot({
      tool: "apply_patch",
      verdict_exclusions: {
        T7: { subject: "v1_host_edit_family_gate", reason: "The host gates it." },
      },
    });

    await expect(loadApplicabilityMatrix(directory)).rejects.toThrow(
      "a verdict exclusion needs the captured host source it rests on",
    );
  });

  test("a subject that rests on an upstream issue is refused when it cites host source", async () => {
    const directory = await matrixRoot({
      tool: "bash",
      verdict_exclusions: {
        T7: {
          subject: "v1_host_process_exit",
          host_source: HOST_SOURCE,
          reason: "The host never exits.",
        },
      },
    });

    await expect(loadApplicabilityMatrix(directory)).rejects.toThrow(
      "v1_host_process_exit rests on an upstream issue, not host source",
    );
  });

  test("the host-source subject reads back what the row cites", async () => {
    const matrix = await loadApplicabilityMatrix(
      await matrixRoot({
        tool: "apply_patch",
        verdict_exclusions: {
          T7: {
            subject: "v1_host_edit_family_gate",
            host_source: HOST_SOURCE,
            reason: "The host picks its edit family from the model id.",
          },
        },
      }),
    );

    expect(matrix?.rows[0].verdict_exclusions?.T7).toEqual({
      subject: "v1_host_edit_family_gate",
      host_source: HOST_SOURCE,
      reason: "The host picks its edit family from the model id.",
    });
  });

  test("a row that already expects failure cannot also exclude part of its verdict", async () => {
    const directory = await matrixRoot({
      tool: "bash",
      trajectories: {
        T1: "applicable",
        T2: "applicable",
        T3: "applicable",
        T4: "applicable",
        T5: "applicable",
        T6: "applicable",
        T7: `expected_fail:${UPSTREAM}`,
      },
      verdict_exclusions: { T7: exclusion() },
    });

    await expect(loadApplicabilityMatrix(directory)).rejects.toThrow(
      "only an applicable row excludes part of its verdict",
    );
  });

  test("the report names the declared exclusion and the one the run applied", () => {
    const excluded = {
      rows: [
        {
          tool: "bash",
          trajectories: {
            T1: "n/a:test",
            T2: "n/a:test",
            T3: "n/a:test",
            T4: "n/a:test",
            T5: "n/a:test",
            T6: "n/a:test",
            T7: "applicable",
          },
          verdict_exclusions: { T7: exclusion() },
        },
      ],
    } as unknown as Parameters<typeof reportTable>[0];

    const report = reportTable(excluded, [
      {
        id: "bash/T7/happy",
        status: "passed",
        exclusions: ["v1_host_process_exit"],
        forensic_dir: "/dev/null",
      },
    ]);

    expect(report.text).toContain(
      `bash | T7 | applicable (excludes v1_host_process_exit: ${UPSTREAM}) | pass`,
    );
    expect(report.text).toContain("bash/T7/happy | passed | excluded v1_host_process_exit");
    expect(report.failed).toBe(false);
  });

  // The V1 host publishes either apply_patch or edit/write, never both, and
  // decides from the model id before the plugin is consulted. The capture that
  // says so is only evidence while the installed host still contains it.
  describe("the edit-family gate is held against the host it is about", () => {
    const SELECTOR =
      'let F=D.modelID.includes("gpt-")&&!D.modelID.includes("oss")&&!D.modelID.includes("gpt-4");if(A.id===xr.id)return F;if(A.id===Hr.id||A.id===vr.id)return!F;';
    const captured = {
      schema_version: 1 as const,
      host_version: "1.18.30",
      observed_run_id: "oc1-1.18.30-test",
      executable: "/opt/opencode1/node_modules/opencode-ai/bin/opencode.exe",
      selector_source: SELECTOR,
      tool_selectors: [{ tool: "apply_patch", source: 'xr=j("apply_patch"' }],
    };

    async function hostBundle(body: string): Promise<string> {
      const directory = await root();
      const path = join(directory, "opencode.exe");
      await writeFile(path, body);
      return path;
    }

    test("a host that still contains every captured fragment satisfies the check", async () => {
      const path = await hostBundle(`prelude\n${SELECTOR}\nmiddle\nxr=j("apply_patch",1)\ntail`);

      expect(await assertV1HostEditFamilyGate(captured, path)).toBeUndefined();
    });

    test("a host that no longer contains the selector ends the run instead", async () => {
      const path = await hostBundle('prelude\nxr=j("apply_patch",1)\ntail');

      await expect(assertV1HostEditFamilyGate(captured, path)).rejects.toThrow(
        "no longer contains its captured selector_source",
      );
    });

    test("the check cannot be satisfied without the host", async () => {
      await expect(assertV1HostEditFamilyGate(captured, undefined)).rejects.toThrow(
        "cannot be checked without the pinned V1 executable",
      );
    });

    test("the committed capture is the one the pinned host version carries", async () => {
      const contract = await loadV1HostEditFamilyGateContract(
        join(import.meta.dir, "..", "contract"),
        await readPinnedV1HostVersion(join(import.meta.dir, "..", "..", "..", "..")),
      );

      expect(contract.selector_source).toContain('modelID.includes("gpt-")');
      expect(contract.tool_selectors.map((selector) => selector.tool).sort()).toEqual([
        "apply_patch",
        "edit",
        "write",
      ]);
    });
  });
});

describe("a parity row compares the two hosts", () => {
  function parityScenario(): ScenarioDefinition {
    return {
      schema_version: 1,
      id: "write/T7/happy",
      tool: "write",
      trajectory: "T7",
      execution: "standalone",
      prompt: "parity",
      turns: [{ label: "finish", response: { kind: "text", content: "done" } }],
      compare_call_id: "call-1",
      comparison: {
        mode: "shape",
        rules: [
          { kind: "field", field: "write_outcome", pattern: "^(?<value>Created new file\\.)$" },
        ],
        expected: { write_outcome: "Created new file." },
      },
    };
  }

  async function allowlistRoot(entries: unknown[]): Promise<string> {
    const directory = await root();
    await writeFile(
      join(directory, "parity-allowlist.json"),
      JSON.stringify({ schema_version: 1, entries }),
    );
    return directory;
  }

  test("a field the two hosts may differ on is allowed by name", async () => {
    const scenario = parityScenario();
    scenario.comparison = {
      mode: "shape",
      rules: [
        { kind: "field", field: "write_outcome", pattern: "^(?<value>Created new file\\.)$" },
        { kind: "field", field: "elapsed", pattern: "^took (?<value>\\d+)ms$" },
      ],
      expected: { write_outcome: "Created new file.", elapsed: 1 },
    };
    const directory = await allowlistRoot([
      {
        scenario: "write/T7/happy",
        tool: "write",
        field: "elapsed",
        reason: "Wall clock is not a product output.",
      },
    ]);

    expect(await validateParityAllowlist(directory, [scenario])).toHaveLength(1);
  });

  test("an allowlist that drops every projected field is refused, because it compares nothing", async () => {
    const directory = await allowlistRoot([
      {
        scenario: "write/T7/happy",
        tool: "write",
        field: "write_outcome",
        reason: "Host transports add presentation text around the tool result.",
      },
    ]);

    await expect(validateParityAllowlist(directory, [parityScenario()])).rejects.toThrow(
      "parity allowlist leaves write/T7/happy comparing nothing between the hosts",
    );
  });

  // The V1 host hands the background-completion reminder back inside the tool
  // result; the V2 host delivers it as a message of its own. Both are right for
  // their host, and the row asks about the tool's behaviour, so the projection
  // reads past the wrapper instead of the allowlist dropping the one field the
  // row exists to compare.
  describe("text a host wraps around the result is projected away", () => {
    const V1_RESULT =
      "Task bash-288f1800f1a302ff: killed\n\n<system-reminder>\n[BACKGROUND BASH COMPLETED]\n- task bash-288f1800f1a302ff (killed)\n</system-reminder>";
    const V2_RESULT = "Task bash-288f1800f1a302ff: killed";

    async function killRules() {
      const scenarios = materializeParityScenarios(
        await loadScenarios(join(import.meta.dir, "..", "scenarios", "bash_kill")),
      );
      const parity = scenarios.find((candidate) => candidate.id === "bash_kill/T7/happy");
      if (parity?.comparison?.mode !== "shape") throw new Error("bash_kill/T7 lost its projection");
      return parity.comparison.rules;
    }

    test("both hosts project to the outcome the tool reported", async () => {
      const rules = await killRules();

      expect(projectText(V1_RESULT, rules)).toEqual({ task_state: "killed" });
      expect(projectText(V2_RESULT, rules)).toEqual({ task_state: "killed" });
    });

    test("the outcome line itself is still read, not swallowed with the reminder", async () => {
      const rules = await killRules();
      const stillRunning = V1_RESULT.replace("a302ff: killed", "a302ff: running");

      expect(() => projectText(stillRunning, rules)).toThrow("projection_unparsed");
    });

    test("a live-descendant warning is not swallowed by the happy reminder", async () => {
      const rules = await killRules();
      const descendants = V1_RESULT.replace(
        "\n</system-reminder>",
        "\n  1 live descendants still running: sleep infinity\n</system-reminder>",
      );
      expect(() => projectText(descendants, rules)).toThrow("projection_unparsed");
    });
  });
});

describe("the kill-deadline line is a typed projection element", () => {
  // AFT appends the task's own kill deadline to every reply that hands a
  // task back and to every bash_watch result. Read the wordings from the
  // plugins' own source, so the pattern cannot drift from what the product
  // prints.
  const rules = [{ kind: "kill_deadline" as const }];
  const startedAtMs = 1_700_000_000_000;
  const nowMs = startedAtMs + 18 * 60_000;
  const running = (hardKill: Record<string, unknown>) => ({
    status: "running",
    started_at: startedAtMs,
    hard_kill: hardKill,
  });
  const render = (data: Record<string, unknown>, role: "primary" | "worker") =>
    taskKillDeadlineText(data, role, nowMs);

  test("every wording the product emits parses absolute time, limit, source and remaining duration", () => {
    const defaultLimit = { limit_ms: 1_800_000, source: "default" };
    expect(projectText(render(running(defaultLimit), "primary"), rules)).toEqual({
      kill_deadline: {
        at: "2023-11-14 22:43:20Z",
        limit: "30 minutes",
        remaining: "12 minutes",
        source: "default",
        worker_renewal: false,
      },
    });
    expect(projectText(render(running(defaultLimit), "worker"), rules)).toEqual({
      kill_deadline: {
        at: "2023-11-14 22:43:20Z",
        limit: "30 minutes",
        remaining: "12 minutes",
        source: "default",
        worker_renewal: true,
      },
    });
    expect(
      projectText(
        taskKillDeadlineText(
          running({ limit_ms: 45_000, source: "timeout" }),
          "worker",
          startedAtMs + 10_000,
        ),
        rules,
      ),
    ).toEqual({
      kill_deadline: {
        at: "2023-11-14 22:14:05Z",
        limit: "45s",
        remaining: "35s",
        source: "timeout",
        worker_renewal: false,
      },
    });
    expect(projectText(taskKillDeadlineText({ status: "running" }, "primary"), rules)).toEqual({
      kill_deadline: { source: "none", worker_renewal: false },
    });
  });

  test("clock omit drops the wall-clock parts but still requires the full sentence", () => {
    const omit = [{ kind: "kill_deadline" as const, clock: "omit" as const }];
    const text = render(running({ limit_ms: 1_800_000, source: "default" }), "primary");
    expect(projectText(text, omit)).toEqual({
      kill_deadline: { limit: "30 minutes", source: "default", worker_renewal: false },
    });
    expect(() => projectText(text.replace("AFT kills", "AFT stops"), omit)).toThrow();
  });

  test("a changed or mangled deadline wording is unparsed, not skipped", () => {
    const line = render(running({ limit_ms: 1_800_000, source: "default" }), "primary");
    expect(() => projectText(line.replace("unless", "until"), rules)).toThrow(
      "projection_unparsed",
    );
    // The caller-timeout wording never carries the default-limit continuation.
    expect(() =>
      projectText(
        line.replace("its default background limit", "the `timeout` you passed"),
        rules,
      ),
    ).toThrow("default-limit continuation");
  });

  test("a scenario row reads the deadline beside the outcome it already projected", () => {
    const watchRules = [
      {
        kind: "field" as const,
        field: "task_state",
        pattern: "^Task bash-[0-9a-f]+: (?<value>running)$",
      },
      { kind: "kill_deadline" as const },
    ];
    const text = `Task bash-1a2b: running\n${render(
      running({ limit_ms: 1_800_000, source: "default" }),
      "primary",
    )}`;
    expect(projectText(text, watchRules)).toEqual({
      task_state: "running",
      kill_deadline: {
        at: "2023-11-14 22:43:20Z",
        limit: "30 minutes",
        remaining: "12 minutes",
        source: "default",
        worker_renewal: false,
      },
    });
  });
});

describe("the run reports a verdict for every row", () => {
  const matrix = {
    rows: [
      {
        tool: "alpha",
        trajectories: {
          T1: "applicable",
          T2: "applicable",
          T3: "expected_fail:https://example.invalid/1",
          T4: "expected_fail:https://example.invalid/1",
          T5: "n/a:no-background-capability",
          T6: "n/a:no-list-surface",
          T7: "n/a:no-list-surface",
        },
      },
    ],
  } as unknown as Parameters<typeof reportTable>[0];

  const result = (
    id: string,
    status: ScenarioResult["status"],
    failure?: ScenarioResult["failure"],
  ): ScenarioResult => ({ id, status, failure, forensic_dir: "/dev/null" });

  test("a failed row does not stop the rows after it, and the counts are stated", () => {
    const report = reportTable(matrix, [
      result("alpha/T1/happy", "failed", { code: "host_failed", message: "first row broke" }),
      result("alpha/T2/happy", "passed"),
      result("alpha/T3/gated", "failed", { code: "host_failed", message: "known upstream gap" }),
      result("alpha/T4/gated", "failed", { code: "host_failed", message: "known upstream gap" }),
    ]);

    expect(report.text).toContain("alpha | T1 | applicable | fail");
    expect(report.text).toContain("alpha | T2 | applicable | pass");
    expect(report.text).toContain("rows: 1 passed, 1 failed, 2 expected-failed, 3 not-applicable (of 7)");
    expect(report.text).toContain("scenarios: 1 passed, 3 failed (of 4)");
    expect(report.failed).toBe(true);
  });

  test("an expected-failure label cannot excuse a harness-integrity failure", () => {
    const productGap = result("alpha/T3/gated", "failed", {
      code: "host_failed",
      message: "known upstream gap",
    });
    const integrityBreach = result("alpha/T3/gated", "failed", {
      code: "executable_provenance",
      message: "producer sidecar missing",
      unsuppressible: true,
    });
    // An unsuppressible product outcome is still a product outcome: the label
    // is about the product, so it may cover this one.
    const unparsedOutput = result("alpha/T3/gated", "failed", {
      code: "projection_unparsed",
      message: "Created new file.",
      unsuppressible: true,
    });

    expect(parentDisposition("expected_fail:https://example.invalid/1", [productGap])).toBe(
      "expected_fail",
    );
    expect(parentDisposition("expected_fail:https://example.invalid/1", [integrityBreach])).toBe(
      "fail",
    );
    expect(parentDisposition("expected_fail:https://example.invalid/1", [unparsedOutput])).toBe(
      "expected_fail",
    );
  });
});

/**
 * Stands in for the task registry while an interrupted foreground call is
 * being aborted. The interrupt has already been answered and the host's client
 * has exited; the plugin's own abort reaches Rust `abortLandsAfterMs` later and
 * records `call_aborted`. If the harness stops the task first (the probe's
 * `cancel`, which signals the task's process group), the task dies of that
 * signal instead and its row carries no abort reason, which is what the
 * failing CI rows recorded.
 */
class InterruptedForegroundTask implements TaskProbe {
  readonly id = "bash-interrupted";
  readonly abortLandsAt: number;
  harnessKilledFirst = false;

  constructor(abortLandsAfterMs: number) {
    this.abortLandsAt = Date.now() + abortLandsAfterMs;
  }

  async states(): Promise<TaskState[]> {
    if (this.harnessKilledFirst) return [{ id: this.id, status: "killed" }];
    if (Date.now() >= this.abortLandsAt) {
      return [{ id: this.id, status: "killed", status_reason: "call_aborted" }];
    }
    return [{ id: this.id, status: "running" }];
  }

  async cancel(id: string): Promise<void> {
    const [task] = await this.states();
    if (id === this.id && task?.status === "running") this.harnessKilledFirst = true;
  }
}

/** The driver's teardown after an interrupted row's host has exited. */
async function tearDownInterruptedRow(probe: InterruptedForegroundTask, deadlineAt: number) {
  const settlement = await waitForAbortSettlement(probe, probe.id, deadlineAt);
  const termination = await new ProcessObserver("bash/T4/abort", probe).cleanupAndConfirm(1_000);
  const failure = callAbortedFailure("bash/T4/abort", termination, settlement);
  return { settlement, termination, failure };
}

describe("interrupted foreground task teardown", () => {
  // Each delay is how long after the host's client exits the plugin's abort
  // lands. Zero is the ordering a quiet machine produces; the others are the
  // ordering a loaded runner produced, injected here instead of hoped for.
  for (const abortLandsAfterMs of [0, 30, 150, 600, 1_500]) {
    test(`an abort landing ${abortLandsAfterMs}ms after the host exits is what the row records`, async () => {
      const probe = new InterruptedForegroundTask(abortLandsAfterMs);
      const { settlement, termination, failure } = await tearDownInterruptedRow(
        probe,
        Date.now() + 5_000,
      );

      expect(probe.harnessKilledFirst).toBe(false);
      expect(settlement.settled).toBe(true);
      expect(termination.tasks).toEqual([
        { id: probe.id, status: "killed", status_reason: "call_aborted" },
      ]);
      expect(failure).toBeUndefined();
    });
  }

  test("an abort that never lands still fails the row once the budget is spent", async () => {
    const probe = new InterruptedForegroundTask(60_000);
    const startedAt = Date.now();
    const { settlement, termination, failure } = await tearDownInterruptedRow(
      probe,
      startedAt + 200,
    );

    expect(settlement.settled).toBe(false);
    expect(settlement.task?.status).toBe("running");
    // The harness still stops the task itself once the budget is gone.
    expect(probe.harnessKilledFirst).toBe(true);
    expect(termination.stopped).toBe(true);
    expect(failure?.message).toContain("did not record status_reason call_aborted");
    expect(failure?.message).toContain("so the harness stopped it");
    expect(Date.now() - startedAt).toBeLessThan(2_000);
  });

  test("a row with no task has nothing to prove", () => {
    const termination = {
      started_at: "",
      completed_at: "",
      tasks: [],
      processes: [],
      stopped: true,
    };
    expect(callAbortedFailure("bash/T4/abort", termination)).toBeUndefined();
  });
});

describe("comparing tool output that carries AFT's status bar", () => {
  const exact = { mode: "exact", expected: "1: alpha\n2: beta\n" } as never;

  test("a trailing status bar is timing, not part of the compared output", () => {
    expect(() =>
      assertComparison("1: alpha\n2: beta\n\n[AFT E? W? | ~D? U? C? | T0]", exact),
    ).not.toThrow();
    expect(() =>
      assertComparison("1: alpha\n2: beta\n\n[AFT E0 W1 | D3 U4 C5 | T6]", exact),
    ).not.toThrow();
    // Output without a final newline gets a two-newline separator.
    expect(() =>
      assertComparison("done\n\n[AFT E? W? | D? U? C? | T0]", {
        mode: "exact",
        expected: "done",
      } as never),
    ).not.toThrow();
  });

  test("anything else still fails the exact comparison", () => {
    expect(() => assertComparison("1: alpha\n2: gamma\n", exact)).toThrow(
      /exact comparison failed/,
    );
    // Only a trailing bar is removed; a bar-like line inside the output counts.
    expect(() =>
      assertComparison("[AFT E? W? | D? U? C? | T0]\n\n1: alpha\n2: beta\n", exact),
    ).toThrow(/exact comparison failed/);
  });
});

describe("cross-host parity with AFT's status bar on one side", () => {
  const scenario = (comparison: unknown) =>
    ({ id: "read/T7/happy", comparison }) as never;
  const exact = scenario({ mode: "exact", expected: "1: alpha\n2: beta\n" });

  test("a status bar on only one host's output is not a parity difference", () => {
    expect(() =>
      assertDualHostParity(
        exact,
        "1: alpha\n2: beta\n",
        "1: alpha\n2: beta\n\n[AFT E? W? | ~D? U? C? | T0]",
        [],
      ),
    ).not.toThrow();
    expect(() =>
      assertDualHostParity(
        exact,
        "1: alpha\n2: beta\n\n[AFT E0 W0 | D0 U0 C0 | T1]",
        "1: alpha\n2: beta\n",
        [],
      ),
    ).not.toThrow();
  });

  test("a real difference still fails and the message shows it", () => {
    let message = "";
    try {
      assertDualHostParity(
        exact,
        "1: alpha\n2: beta\n",
        "1: alpha\n2: gamma\n\n[AFT E? W? | ~D? U? C? | T0]",
        [],
      );
    } catch (error) {
      message = (error as Error).message;
    }
    expect(message).toContain("exact V1/V2 parity mismatch");
    expect(message).toContain('- "2: beta"');
    expect(message).toContain('+ "2: gamma"');
    // A trailing newline difference is a difference too.
    expect(() => assertDualHostParity(exact, "1: alpha\n", "1: alpha", [])).toThrow(
      /exact V1\/V2 parity mismatch/,
    );
  });

  test("projected parity also leaves the status bar out", () => {
    const projected = scenario({
      mode: "shape",
      expected: {},
      rules: [{ kind: "field", field: "line", pattern: "^(?<line>\\d+): .*$", max_lines: 1 }],
    });
    expect(() =>
      assertDualHostParity(projected, "1: alpha", "1: alpha\n\n[AFT E? W? | D? U? C? | T0]", []),
    ).not.toThrow();
    expect(() =>
      assertDualHostParity(projected, "1: alpha", "2: alpha\n\n[AFT E? W? | D? U? C? | T0]", []),
    ).toThrow(/projected V1\/V2 parity mismatch/);
  });

  test("the difference summary names the first differing line", () => {
    expect(describeTextDifference("a\nb\nc", "a\nx\nc")).toBe(
      'first difference at line 2\n  "a"\n- "b"\n+ "x"\n  "c"',
    );
    expect(describeTextDifference("a", "a")).toBe("(texts are identical)");
  });
});

describe("a row that needs the callgraph waits for it", () => {
  const gatedTurn = (timeoutMs: number): ScriptedTurn => ({
    label: "call-tool",
    await_ready: { subject: "callgraph", timeout_ms: timeoutMs },
    response: { kind: "tool_calls", calls: [call({ name: "aft_callgraph" })] },
  });

  test("the call is held until the probe reports ready", async () => {
    let polls = 0;
    const record = await awaitTurnReadiness(
      gatedTurn(5_000),
      { callgraph: async () => ++polls >= 3 },
      5,
    );
    expect(polls).toBe(3);
    expect(record).toMatchObject({ subject: "callgraph", turn: "call-tool", polls: 3 });
  });

  test("a store that never becomes ready fails the row by name", async () => {
    let polls = 0;
    const error = await expectCode(
      () =>
        awaitTurnReadiness(
          gatedTurn(60),
          {
            callgraph: async () => {
              polls += 1;
              return false;
            },
          },
          10,
        ),
      "callgraph_never_ready",
    );
    expect(error.message).toContain("callgraph never became ready within 60ms");
    // It kept checking until the budget ran out rather than giving up at once.
    expect(polls).toBeGreaterThan(1);
  });

  test("a turn without the declaration does not wait or probe", async () => {
    const turn: ScriptedTurn = {
      label: "call-tool",
      response: { kind: "tool_calls", calls: [call({ name: "aft_callgraph" })] },
    };
    const record = await awaitTurnReadiness(turn, {
      callgraph: async () => {
        throw new Error("an undeclared turn consulted the readiness probe");
      },
    });
    expect(record).toBeUndefined();
    expect(readinessBudgetMs([turn])).toBe(0);
  });

  test("the host budget grows by every declared wait", () => {
    expect(readinessBudgetMs([gatedTurn(1_000), gatedTurn(2_000)])).toBe(3_000);
  });

  test("the store counts as ready only once AFT has published it marked ready", async () => {
    const storage = await root();
    expect(await callgraphStorePublished(storage)).toBe(false);
    const directory = join(storage, "callgraph", "key123");
    await mkdir(directory, { recursive: true });
    const generation = "key123.g1.1.sqlite";
    const database = new Database(join(directory, generation));
    database.exec("CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT NOT NULL)");
    database.exec("INSERT INTO meta (k, v) VALUES ('ready', '0')");
    // A generation file with no pointer to it is a build still in progress.
    expect(await callgraphStorePublished(storage)).toBe(false);
    await writeFile(join(directory, "key123.current"), `${generation}\n`);
    // Published but not yet marked ready.
    expect(await callgraphStorePublished(storage)).toBe(false);
    database.exec("UPDATE meta SET v = '1' WHERE k = 'ready'");
    database.close();
    expect(await callgraphStorePublished(storage)).toBe(true);
    // A pointer naming a generation that is gone is not ready.
    await writeFile(join(directory, "key123.current"), "key123.g2.1.sqlite\n");
    expect(await callgraphStorePublished(storage)).toBe(false);
  });
});

describe("abort controls wait for the call they interrupt", () => {
  // A scenario that interrupts a running call: the abort control is sent
  // after the turn that hands the host the call.
  function abortRow(control: Partial<ApiControlPlan>): ScenarioDefinition {
    return {
      schema_version: 1,
      id: "inspect/T4/abort",
      tool: "inspect",
      trajectory: "T4",
      execution: "shared-server",
      prompt: "start and abort",
      turns: [
        {
          label: "call-tool",
          response: {
            kind: "tool_calls",
            calls: [
              {
                id: "inspect-abort",
                name: "aft_inspect",
                arguments: {},
                non_mutating_evidence: { reason: "aborted inspection does not mutate" },
                outlives_result: true,
              },
            ],
          },
        },
        { label: "finish", response: { kind: "text", content: "done" } },
      ],
      controls: [
        {
          id: "abort-control",
          after_turn: "call-tool",
          method: "POST",
          path: "/api/session/{{session_id}}/interrupt",
          purpose: "abort",
          ...control,
        },
      ],
    };
  }

  test("an abort control with no gate is refused", () => {
    expect(() => validateScenarioDefinition(abortRow({}), "inspect")).toThrow(
      "declares neither wait_for_task nor wait_for_tool_call",
    );
  });

  test("a tool-call gate must name a call its own turn issues", () => {
    expect(() =>
      validateScenarioDefinition(
        abortRow({ wait_for_tool_call: { call_id: "another-call", timeout_ms: 1_000 } }),
        "inspect",
      ),
    ).toThrow("waits for call another-call, which turn call-tool does not issue");
    expect(() =>
      validateScenarioDefinition(
        abortRow({ wait_for_tool_call: { call_id: "inspect-abort", timeout_ms: 0 } }),
        "inspect",
      ),
    ).toThrow("has invalid wait_for_tool_call");
    expect(() =>
      validateScenarioDefinition(
        abortRow(JSON.parse('{"wait_for_tool_call": null}')),
        "inspect",
      ),
    ).toThrow("has invalid wait_for_tool_call");
    expect(() =>
      validateScenarioDefinition(
        abortRow({ wait_for_tool_call: { call_id: "inspect-abort", timeout_ms: 1_000 } }),
        "inspect",
      ),
    ).not.toThrow();
  });

  test("the start of a call is read from the host's session.tool.called", () => {
    const events: HostEvent[] = [
      { type: "session.step.started", data: { sessionID: "s" } },
      { type: "session.tool.input.started", data: { sessionID: "s", id: "inspect-abort" } },
      { type: "session.tool.called", data: { sessionID: "s", id: "other-call" } },
    ];
    // Streaming the call's input is not running it.
    expect(findToolCalled(events, "inspect-abort")).toBeUndefined();
    const started = { type: "session.tool.called", data: { sessionID: "s", id: "inspect-abort" } };
    expect(findToolCalled([...events, started], "inspect-abort")).toBe(started);
    // Some host versions name the call identifier callID rather than id.
    const renamed = { type: "session.tool.called", data: { sessionID: "s", callID: "inspect-abort" } };
    expect(findToolCalled([...events, renamed], "inspect-abort")).toBe(renamed);
  });

  test("the gate holds until the host reports the call started, and gives up without it", async () => {
    const recorder = new HostEventRecorder();
    expect(await recorder.awaitToolCalled("inspect-abort", 60)).toBeUndefined();

    let settled = false;
    const waiting = recorder.awaitToolCalled("inspect-abort", 5_000).then((event) => {
      settled = true;
      return event;
    });
    recorder.events.push({ type: "session.tool.called", data: { id: "other-call" } });
    await Bun.sleep(100);
    expect(settled).toBe(false);
    const started = { type: "session.tool.called", data: { id: "inspect-abort" } };
    recorder.events.push(started);
    expect(await waiting).toBe(started);
  });

  test("a control whose call never starts fails by name and records what the stream carried", async () => {
    const control: ApiControlPlan = {
      id: "abort-control",
      after_turn: "call-tool",
      wait_for_tool_call: { call_id: "inspect-abort", timeout_ms: 60 },
      method: "POST",
      path: "/api/session/{{session_id}}/interrupt",
      purpose: "abort",
    };
    const recorder = new HostEventRecorder();
    recorder.events.push({ type: "session.execution.interrupted", data: {} });
    const recorded: Array<{ name: string; value: unknown }> = [];
    const record = async (name: string, value: unknown) => {
      recorded.push({ name, value });
    };

    await expect(
      awaitControlToolCall("inspect/T4/abort", control, recorder, record),
    ).rejects.toThrow("no session.tool.called for it arrived; the control was not sent");
    expect(recorded.map((entry) => entry.name)).toEqual([
      "control-abort-control-tool-readiness.json",
    ]);
    expect(recorded[0]?.value).toMatchObject({
      call_id: "inspect-abort",
      observed: null,
      observed_types: ["session.execution.interrupted"],
    });

    recorder.events.push({ type: "session.tool.called", data: { id: "inspect-abort" } });
    await awaitControlToolCall("inspect/T4/abort", control, recorder, record);
    expect(recorded[1]?.value).toMatchObject({ observed: { data: { id: "inspect-abort" } } });
  });
});
