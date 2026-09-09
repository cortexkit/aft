import { cp, mkdir, mkdtemp, writeFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import { basename, join, resolve } from "node:path";

import { fail } from "./errors.js";
import { runCommand } from "./util.js";

export interface ScenarioIsolation {
  root: string;
  project: string;
  home: string;
  config: string;
  data: string;
  state: string;
  cache: string;
  runtime: string;
  temp: string;
  env: NodeJS.ProcessEnv;
  plugin_log: string;
  host_config: string;
}

export async function createScenarioIsolation(options: {
  parent: string;
  scenarioId: string;
  fixture?: string;
  pluginTarball: string;
  mockBaseUrl: string;
  model?: string;
  projectConfig?: Record<string, unknown>;
}): Promise<ScenarioIsolation> {
  const safeId = options.scenarioId.replaceAll(/[^a-zA-Z0-9_.-]+/g, "-");
  await mkdir(options.parent, { recursive: true });
  const root = await mkdtemp(join(options.parent, `${safeId}-`));
  const paths = {
    project: join(root, "project"),
    home: join(root, "home"),
    config: join(root, "xdg-config"),
    data: join(root, "xdg-data"),
    state: join(root, "xdg-state"),
    cache: join(root, "xdg-cache"),
    runtime: join(root, "xdg-runtime"),
    temp: join(root, "tmp"),
  };
  await Promise.all(Object.values(paths).map((path) => mkdir(path, { recursive: true })));
  if (options.fixture) await cp(options.fixture, paths.project, { recursive: true });
  const gitInit = await runCommand("git", ["init", "-q"], {
    cwd: paths.project,
    timeoutMs: 5_000,
  });
  if (gitInit.exit_code !== 0) {
    fail("fixture_invalid", `git init failed for ${options.scenarioId}`, { output: gitInit });
  }
  const gitAdd = await runCommand("git", ["add", "-A"], {
    cwd: paths.project,
    timeoutMs: 5_000,
  });
  if (gitAdd.exit_code !== 0) {
    fail("fixture_invalid", `git add failed for ${options.scenarioId}`, { output: gitAdd });
  }
  const gitCommit = await runCommand(
    "git",
    [
      "-c",
      "user.name=OpenCode 2 Harness",
      "-c",
      "user.email=opencode2-harness@example.invalid",
      "commit",
      "--allow-empty",
      "-qm",
      "scenario baseline",
    ],
    { cwd: paths.project, timeoutMs: 5_000 },
  );
  if (gitCommit.exit_code !== 0) {
    fail("fixture_invalid", `git commit failed for ${options.scenarioId}`, { output: gitCommit });
  }

  const pluginUrl = pathToFileURL(resolve(options.pluginTarball)).href;
  if (!pluginUrl.startsWith("file://") || !pluginUrl.endsWith(".tgz")) {
    fail("plugin_source_invalid", `plugin source is not a file:// tarball: ${pluginUrl}`, {}, true);
  }
  const opencodeDir = join(paths.config, "opencode");
  await mkdir(opencodeDir, { recursive: true });
  const hostConfig = join(opencodeDir, "opencode.json");
  await writeFile(
    hostConfig,
    `${JSON.stringify(
      {
        $schema: "https://opencode.ai/config.json",
        plugin: [pluginUrl],
        provider: {
          mock: {
            api: "openai",
            name: "deterministic aimock",
            options: { baseURL: `${options.mockBaseUrl.replace(/\/$/, "")}/v1` },
            models: { [options.model ?? "mock-model"]: { name: "Deterministic mock" } },
          },
        },
      },
      null,
      2,
    )}\n`,
  );
  if (options.projectConfig) {
    await writeFile(
      join(paths.project, "aft.jsonc"),
      `${JSON.stringify(options.projectConfig, null, 2)}\n`,
    );
  }
  const pluginLog = join(paths.data, "cortexkit", "aft", "logs", "aft-plugin.log");
  await mkdir(join(paths.data, "cortexkit", "aft", "logs"), { recursive: true });
  await writeFile(pluginLog, "");

  const env: NodeJS.ProcessEnv = {
    ...process.env,
    HOME: paths.home,
    XDG_CONFIG_HOME: paths.config,
    XDG_DATA_HOME: paths.data,
    XDG_STATE_HOME: paths.state,
    XDG_CACHE_HOME: paths.cache,
    XDG_RUNTIME_DIR: paths.runtime,
    TMPDIR: paths.temp,
    OPENCODE_DISABLE_DEFAULT_PLUGINS: "true",
    OPENAI_API_KEY: "sk-opencode2-harness",
    AFT_E2E_PLUGIN_LOG: pluginLog,
    AFT_E2E_ISOLATION_ROOT: root,
    AFT_CACHE_DIR: join(paths.cache, "aft-cache"),
    AFT_STORAGE_DIR: join(paths.data, "cortexkit", "aft"),
  };
  delete env.OPENCODE_CONFIG_CONTENT;
  delete env.OPENCODE_SERVER;
  delete env.OPENCODE_SERVER_PASSWORD;

  assertScenarioIsolation(env, root);
  return {
    root,
    ...paths,
    env,
    plugin_log: pluginLog,
    host_config: hostConfig,
  };
}

export function assertScenarioIsolation(env: NodeJS.ProcessEnv, root: string): void {
  const required = [
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
  ] as const;
  for (const key of required) {
    const value = env[key];
    if (
      !value ||
      !(resolve(value) === resolve(root) || resolve(value).startsWith(`${resolve(root)}/`))
    ) {
      fail(
        "scenario_invalid",
        `scenario isolation does not contain ${key}`,
        { key, value, root },
        true,
      );
    }
  }
  if (env.OPENCODE_DISABLE_DEFAULT_PLUGINS !== "true") {
    fail("scenario_invalid", "OPENCODE_DISABLE_DEFAULT_PLUGINS must be true", {}, true);
  }
}

export function assertPluginLoadEvidence(options: {
  log: string;
  pluginTarball: string;
  expectedVersion: string;
  expectedEntry?: "root" | "server";
}): void {
  const source = pathToFileURL(resolve(options.pluginTarball)).href;
  const tarballName = basename(options.pluginTarball);
  const sourceLine = options.log
    .split(/\r?\n/)
    .find(
      (line) => line.includes(source) || (line.includes("file:") && line.includes(tarballName)),
    );
  if (!sourceLine) {
    fail(
      "plugin_source_invalid",
      `resolution log does not name tarball source ${source}`,
      { expected_source: source },
      true,
    );
  }
  const entryPattern =
    options.expectedEntry === "root"
      ? /(?:resolvedEntry|resolved entry|entry)[=: ].*(?:dist\/index\.js|\/index\.(?:m?js|ts))/i
      : /(?:resolvedEntry|resolved entry|entry)[=: ].*(?:\/server|server\.(?:m?js|ts))/i;
  if (!entryPattern.test(options.log)) {
    fail(
      "plugin_source_invalid",
      `plugin load log does not prove the ${options.expectedEntry === "root" ? "root" : "./server"} entry`,
      {},
      true,
    );
  }
  const escapedVersion = options.expectedVersion.replaceAll(/[.*+?^${}()|[\]\\]/g, "\\$&");
  if (
    !new RegExp(
      `PLUGIN_VERSION[^\\n]*${escapedVersion}|plugin(?: version)?[=: ]+${escapedVersion}`,
      "i",
    ).test(options.log)
  ) {
    fail(
      "plugin_source_invalid",
      `plugin load log does not prove PLUGIN_VERSION ${options.expectedVersion}`,
      { expected_version: options.expectedVersion },
      true,
    );
  }
}
