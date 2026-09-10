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

function materializeProviderConfig(value: unknown, mockBaseUrl: string): unknown {
  if (typeof value === "string") {
    return value
      .replaceAll("{{AIMOCK_BASE_URL}}", mockBaseUrl)
      .replaceAll("$AIMOCK_BASE_URL", mockBaseUrl);
  }
  if (Array.isArray(value))
    return value.map((entry) => materializeProviderConfig(entry, mockBaseUrl));
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).map(([key, entry]) => [
        key,
        materializeProviderConfig(entry, mockBaseUrl),
      ]),
    );
  }
  return value;
}

export async function createScenarioIsolation(options: {
  parent: string;
  scenarioId: string;
  fixture?: string;
  pluginTarball: string;
  pluginDirectory: string;
  pluginVersion: string;
  binaryPath?: string;
  mockBaseUrl: string;
  model?: string;
  projectConfig?: Record<string, unknown>;
  providerConfig?: Record<string, unknown>;
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
  const pluginWrapper = join(paths.config, "aft-opencode-wrapper");
  const serverEntry = pathToFileURL(
    join(options.pluginDirectory, "dist", "entry", "server.js"),
  ).href;
  const wrapperModule = (resolvedEntry: string) =>
    `import { appendFileSync } from "node:fs";\n` +
    `import plugin from ${JSON.stringify(resolvedEntry)};\n` +
    `const pluginLog = process.env.AFT_E2E_PLUGIN_LOG;\n` +
    `if (pluginLog) appendFileSync(pluginLog, ${JSON.stringify(
      `plugin source=${pluginUrl} resolvedEntry=${resolvedEntry} PLUGIN_VERSION=${options.pluginVersion}\n`,
    )});\n` +
    `const effect = plugin.effect;\n` +
    `export default { ...plugin, effect: effect && ((context) => {\n` +
    `  if (pluginLog) appendFileSync(pluginLog, "context keys=" + Object.keys(context).sort().join(",") + "\\n");\n` +
    `  return effect(context);\n` +
    `}) };\n`;
  await mkdir(pluginWrapper, { recursive: true });
  await Promise.all([
    writeFile(
      join(pluginWrapper, "package.json"),
      `${JSON.stringify({
        name: "aft-opencode-e2e-wrapper",
        private: true,
        type: "module",
        main: "./index.mjs",
      })}\n`,
    ),
    writeFile(join(pluginWrapper, "index.mjs"), wrapperModule(serverEntry)),
  ]);
  const pluginDirectoryUrl = pathToFileURL(pluginWrapper).href;
  const providerConfig = options.providerConfig
    ? materializeProviderConfig(options.providerConfig, options.mockBaseUrl)
    : {
        mock: {
          api: "openai",
          name: "deterministic aimock",
          options: { baseURL: `${options.mockBaseUrl.replace(/\/$/, "")}/v1` },
          models: { [options.model ?? "mock-model"]: { name: "Deterministic mock" } },
        },
      };
  const opencodeDir = join(paths.config, "opencode");
  await mkdir(opencodeDir, { recursive: true });
  const hostConfig = join(opencodeDir, "opencode.json");
  await writeFile(
    hostConfig,
    `${JSON.stringify(
      {
        $schema: "https://opencode.ai/config.json",
        plugin: [pluginDirectoryUrl],
        providers: providerConfig,
      },
      null,
      2,
    )}\n`,
  );
  await mkdir(join(paths.project, ".cortexkit"), { recursive: true });
  await writeFile(
    join(paths.project, ".cortexkit", "aft.jsonc"),
    `${JSON.stringify(
      {
        tool_surface: "all",
        semantic_search: true,
        search_index: true,
        ...options.projectConfig,
      },
      null,
      2,
    )}\n`,
  );
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
    AFT_BINARY_PATH: options.binaryPath ?? process.env.AFT_BINARY_PATH,
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
