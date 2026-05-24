import { type ChildProcess, spawn } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { createRpcClient, type RpcClient } from "./rpc-client";

const REPO_ROOT = resolve(import.meta.dir, "../../..");
const require_ = createRequire(import.meta.url);

function compareSemver(a: string, b: string): number {
  const left = a.split(".").map((part) => Number(part));
  const right = b.split(".").map((part) => Number(part));
  for (let index = 0; index < Math.max(left.length, right.length); index += 1) {
    const diff = (left[index] ?? 0) - (right[index] ?? 0);
    if (diff !== 0) return diff;
  }
  return 0;
}

function resolvePiPackageJson(): string {
  try {
    return require_.resolve("@earendil-works/pi-coding-agent/package.json");
  } catch {
    const bunModules = join(REPO_ROOT, "node_modules/.bun");
    const prefix = "@mariozechner+pi-coding-agent@";
    const candidates = readdirSync(bunModules, { withFileTypes: true })
      .filter((entry) => entry.isDirectory() && entry.name.startsWith(prefix))
      .map((entry) => {
        const version = entry.name.slice(prefix.length).split("+")[0] ?? "0.0.0";
        return { name: entry.name, version };
      })
      .sort((a, b) => compareSemver(b.version, a.version));
    const best = candidates[0];
    if (best === undefined) {
      throw new Error(`Could not locate @earendil-works/pi-coding-agent under ${bunModules}`);
    }
    return join(bunModules, best.name, "node_modules/@earendil-works/pi-coding-agent/package.json");
  }
}

export function resolvePiCli(): string {
  return join(dirname(resolvePiPackageJson()), "dist/cli.js");
}

export function resolvePiPluginDir(): string {
  return join(REPO_ROOT, "packages/pi-plugin");
}

export interface PiSpawnOptions {
  mockProviderURL: string;
  aftPluginDir: string;
  configDir: string;
  workdir: string;
  extraArgs?: string[];
}

function childEnv(configDir: string): Record<string, string> {
  const result: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (value === undefined || key === "NODE_ENV") continue;
    result[key] = value;
  }
  result.HOME = configDir;
  result.PI_CODING_AGENT_DIR = join(configDir, ".pi", "agent");
  result.XDG_CONFIG_HOME = join(configDir, "config");
  result.XDG_DATA_HOME = join(configDir, "data");
  result.XDG_CACHE_HOME = join(configDir, "cache");
  result.OPENAI_API_KEY = "sk-mock";
  result.PI_OFFLINE = "1";
  result.PI_SKIP_VERSION_CHECK = "1";
  // Prepend BOTH target/release and target/debug to PATH so the Pi RPC
  // tests find the aft binary regardless of which build the surrounding
  // CI job produced:
  //   - dedicated `pi-rpc-e2e` job:        cargo build --release  → target/release/aft
  //   - workspace `Test` / `Test (macOS)`: cargo test --workspace → target/debug/aft
  //
  // Locally either may exist; release takes precedence to match the
  // dedicated CI job's behavior. Mirrors the same fallback pattern used by
  // packages/opencode-plugin/src/__tests__/e2e/helpers.ts and
  // packages/pi-plugin/src/__tests__/e2e/helpers.ts (TARGET_DEBUG_BINARY).
  const sep = process.platform === "win32" ? ";" : ":";
  result.PATH = [
    join(REPO_ROOT, "target", "release"),
    join(REPO_ROOT, "target", "debug"),
    result.PATH ?? "",
  ].join(sep);
  return result;
}

function writeConfigs(opts: PiSpawnOptions): string {
  const agentDir = join(opts.configDir, ".pi", "agent");
  const extensionsDir = join(agentDir, "extensions");
  mkdirSync(extensionsDir, { recursive: true });
  mkdirSync(join(opts.configDir, "config"), { recursive: true });
  mkdirSync(join(opts.configDir, "data"), { recursive: true });
  mkdirSync(join(opts.configDir, "cache"), { recursive: true });

  const distEntry = join(opts.aftPluginDir, "dist", "index.js");
  if (!existsSync(distEntry)) {
    throw new Error(`${distEntry} is missing. Run: cd packages/pi-plugin && bun run build`);
  }

  const installedPluginDir = join(extensionsDir, "aft-pi");
  if (!existsSync(installedPluginDir)) symlinkSync(opts.aftPluginDir, installedPluginDir, "dir");

  const template = readFileSync(join(import.meta.dir, "../fixtures/models.json.tmpl"), "utf8");
  writeFileSync(
    join(agentDir, "models.json"),
    template.replace("${MOCK_URL}", opts.mockProviderURL),
  );
  writeFileSync(
    join(agentDir, "settings.json"),
    JSON.stringify(
      {
        packages: [`file:${installedPluginDir}`],
        defaultProvider: "mock",
        defaultModel: "mock-model",
        enabledModels: ["mock/mock-model"],
        compaction: { enabled: false },
        retry: { enabled: false },
        quietStartup: true,
        enableInstallTelemetry: false,
      },
      null,
      2,
    ),
  );
  writeFileSync(
    join(agentDir, "aft.jsonc"),
    readFileSync(join(import.meta.dir, "../fixtures/aft-pi-config.jsonc"), "utf8"),
  );
  return agentDir;
}

export function spawnPiRpc(opts: PiSpawnOptions): { child: ChildProcess; client: RpcClient } {
  const agentDir = writeConfigs(opts);
  const child = spawn(
    "node",
    [
      resolvePiCli(),
      "--mode",
      "rpc",
      "--provider",
      "mock",
      "--model",
      "mock/mock-model",
      "--no-session",
      "--session-dir",
      join(opts.configDir, "sessions"),
      ...(opts.extraArgs ?? []),
    ],
    {
      cwd: opts.workdir,
      env: { ...childEnv(opts.configDir), PI_CODING_AGENT_DIR: agentDir },
      stdio: ["pipe", "pipe", "pipe"],
    },
  );

  let stderr = "";
  child.stderr?.on("data", (chunk) => {
    stderr += String(chunk);
  });
  child.once("exit", (code, signal) => {
    if (code !== 0 && signal !== "SIGTERM" && stderr.length > 0) {
      process.stderr.write(`Pi RPC stderr:\n${stderr}\n`);
    }
  });

  return { child, client: createRpcClient(child) };
}
