import { copyFile, cp, mkdir, mkdtemp, readFile, symlink, writeFile } from "node:fs/promises";
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

/** The package the V1 host installs into every config directory it loads. */
export const HOST1_CONFIG_DEPENDENCY = "@opencode-ai/plugin";

async function readJsonObject(path: string): Promise<Record<string, unknown> | undefined> {
  try {
    const value: unknown = JSON.parse(await readFile(path, "utf8"));
    return value && typeof value === "object" ? (value as Record<string, unknown>) : undefined;
  } catch {
    return undefined;
  }
}

function dependencyNames(manifest: Record<string, unknown> | undefined): string[] {
  const dependencies = manifest?.dependencies;
  return dependencies && typeof dependencies === "object" ? Object.keys(dependencies) : [];
}

/**
 * Gives the V1 host's config directory the dependency install it would
 * otherwise run itself at every start.
 *
 * On each start the V1 host (opencode-ai 1.18.x) forks an npm install of
 * `@opencode-ai/plugin` into every config directory it loads, and its plugin
 * loader waits for that install before importing any plugin. Each scenario
 * has a fresh XDG_CONFIG_HOME, so every V1 start downloaded the package again:
 * on CI that took 25-30 s of the row's 45 s host budget, and a row that needed
 * a little more than the rest (bash/T7/happy) was killed just after its first
 * tool call, which read like the host freezing around the plugin's bash path.
 *
 * The host skips the install when the directory already has a node_modules
 * and a package-lock.json whose root lists every dependency its package.json
 * and its own request name. So the image installs the package once, and each
 * scenario gets its manifest and lock copied and its node_modules linked to
 * that install. The link is never written through: if the host ever decided
 * the directory was dirty, its install would fail on the read-only target and
 * the host logs "background dependency install failed" instead of hanging.
 */
export async function seedHostConfigDependencies(
  configDirectory: string,
  template: string,
): Promise<void> {
  const manifest = await readJsonObject(join(template, "package.json"));
  const lock = await readJsonObject(join(template, "package-lock.json"));
  const lockRoot = (lock?.packages as Record<string, Record<string, unknown>> | undefined)?.[""];
  const installed = await readJsonObject(
    join(template, "node_modules", HOST1_CONFIG_DEPENDENCY, "package.json"),
  );
  // Anything short of all three and the host would reinstall anyway; saying so
  // here keeps a broken image from quietly bringing the slow start back.
  if (
    !dependencyNames(manifest).includes(HOST1_CONFIG_DEPENDENCY) ||
    !dependencyNames(lockRoot).includes(HOST1_CONFIG_DEPENDENCY) ||
    !installed
  ) {
    fail(
      "host_failed",
      `${template} is not an installed ${HOST1_CONFIG_DEPENDENCY}: it needs package.json and package-lock.json listing it, and node_modules holding it`,
      { template },
      true,
    );
  }
  await Promise.all([
    copyFile(join(template, "package.json"), join(configDirectory, "package.json")),
    copyFile(join(template, "package-lock.json"), join(configDirectory, "package-lock.json")),
    symlink(join(template, "node_modules"), join(configDirectory, "node_modules"), "dir"),
  ]);
}

export async function createScenarioIsolation(options: {
  parent: string;
  scenarioId: string;
  fixture?: string;
  pluginTarball: string;
  pluginDirectory: string;
  pluginVersion: string;
  hostGeneration: "v1" | "v2";
  binaryPath?: string;
  mockBaseUrl: string;
  projectConfig?: Record<string, unknown>;
  /** The provider object this host generation was observed to accept. */
  providerConfig: Record<string, unknown>;
  /** The opencode.json key that provider object goes under. */
  providerConfigKey: string;
  /**
   * A directory holding the host's own config-directory dependencies, already
   * installed (see seedHostConfigDependencies). Only the V1 host uses it.
   */
  hostDependencies?: string;
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
  const pluginEntry = pathToFileURL(
    options.hostGeneration === "v2"
      ? join(options.pluginDirectory, "dist", "entry", "server.js")
      : join(options.pluginDirectory, "dist", "index.js"),
  ).href;
  // The wrapper has to be the shape its host accepts, and the two hosts do not
  // agree: V1 takes a plugin function and refuses anything else with "Plugin
  // export is not a function", while V2 takes an object carrying `effect`.
  // Handing every host the object form loads nothing on V1 — the import runs,
  // so the source line still reaches the log, but no tool is ever registered.
  const wrapperPreamble = (resolvedEntry: string) =>
    `import { appendFileSync } from "node:fs";\n` +
    `import plugin from ${JSON.stringify(resolvedEntry)};\n` +
    `const pluginLog = process.env.AFT_E2E_PLUGIN_LOG;\n` +
    `if (pluginLog) appendFileSync(pluginLog, ${JSON.stringify(
      `plugin source=${pluginUrl} resolvedEntry=${resolvedEntry} PLUGIN_VERSION=${options.pluginVersion}\n`,
    )});\n` +
    `const recordContext = (context) => {\n` +
    `  if (pluginLog) appendFileSync(pluginLog, "context keys=" + Object.keys(context ?? {}).sort().join(",") + "\\n");\n` +
    `};\n`;
  const wrapperModule = (resolvedEntry: string) =>
    options.hostGeneration === "v1"
      ? `${wrapperPreamble(resolvedEntry)}` +
        `export default (context) => {\n` +
        `  recordContext(context);\n` +
        `  return plugin(context);\n` +
        `};\n`
      : `${wrapperPreamble(resolvedEntry)}` +
        `const effect = plugin.effect;\n` +
        `export default { ...plugin, effect: effect && ((context) => {\n` +
        `  recordContext(context);\n` +
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
    writeFile(join(pluginWrapper, "index.mjs"), wrapperModule(pluginEntry)),
  ]);
  const pluginDirectoryUrl = pathToFileURL(pluginWrapper).href;
  const providerConfig = materializeProviderConfig(options.providerConfig, options.mockBaseUrl);
  const opencodeDir = join(paths.config, "opencode");
  await mkdir(opencodeDir, { recursive: true });
  if (options.hostGeneration === "v1" && options.hostDependencies) {
    await seedHostConfigDependencies(opencodeDir, options.hostDependencies);
  }
  const hostConfig = join(opencodeDir, "opencode.json");
  await writeFile(
    hostConfig,
    `${JSON.stringify(
      {
        $schema: "https://opencode.ai/config.json",
        plugin: [pluginDirectoryUrl],
        // Which key holds the providers is part of the captured contract, not a
        // harness choice: V2 reads `providers`, V1 reads `provider` and logs
        // `["providers"]` as "unsupported" before leaving itself with no
        // provider at all, so the run dies on a model it cannot resolve.
        [options.providerConfigKey]: providerConfig,
      },
      null,
      2,
    )}\n`,
  );
  // User tier: an explicit empty disable list registers every tool, including
  // aft_delete and aft_move, which are off by default. A project config cannot
  // do this, because a project may disable tools but never enable one.
  await mkdir(join(paths.config, "cortexkit"), { recursive: true });
  await writeFile(
    join(paths.config, "cortexkit", "aft.jsonc"),
    `${JSON.stringify({ disabled_tools: [] }, null, 2)}\n`,
  );
  await mkdir(join(paths.project, ".cortexkit"), { recursive: true });
  await writeFile(
    join(paths.project, ".cortexkit", "aft.jsonc"),
    `${JSON.stringify({ ...options.projectConfig }, null, 2)}\n`,
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
    OPENCODE_DB: "opencode2.db",
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
  if (env.OPENCODE_DB !== "opencode2.db") {
    fail("scenario_invalid", "OPENCODE_DB must be opencode2.db", {}, true);
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
