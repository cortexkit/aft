import { mkdirSync, realpathSync, rmSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

/**
 * Root for the isolated Pi environments. Not the OS temp dir: the product
 * records no undo snapshot for mutations under a system temp path (release
 * binaries enforce that unconditionally), so a workspace under /tmp would make
 * every undo assertion in these suites fail for a reason unrelated to Pi. The
 * user cache directory is the same out-of-repo, out-of-temp location the Pi
 * plugin e2e helpers use.
 */
function isolatedEnvRoot(): string {
  const override = process.env.AFT_PI_RPC_TEST_ROOT;
  if (override) return override;
  return join(homedir(), ".cache", "aft-pi-rpc-e2e");
}

export interface PiIsolatedEnv {
  baseDir: string;
  configDir: string;
  dataDir: string;
  cacheDir: string;
  workdir: string;
  agentDir: string;
  pluginDir: string;
}

export function createPiIsolatedEnv(sharedDataDir?: string): PiIsolatedEnv {
  const unique = `aft-pi-rpc-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  const baseDirRaw = join(isolatedEnvRoot(), unique);
  mkdirSync(baseDirRaw, { recursive: true });
  const baseDir = realpathSync(baseDirRaw);
  const configDir = join(baseDir, "config");
  const dataDir = sharedDataDir ? realpathSync(sharedDataDir) : join(baseDir, "data");
  const cacheDir = join(baseDir, "cache");
  const workdir = join(baseDir, "work");
  const agentDir = join(configDir, ".pi", "agent");
  const pluginDir = join(agentDir, "extensions", "aft-pi");

  for (const dir of [
    configDir,
    dataDir,
    cacheDir,
    workdir,
    agentDir,
    join(agentDir, "extensions"),
  ]) {
    mkdirSync(dir, { recursive: true });
  }

  return {
    baseDir: realpathSync(baseDir),
    configDir: realpathSync(configDir),
    dataDir: realpathSync(dataDir),
    cacheDir: realpathSync(cacheDir),
    workdir: realpathSync(workdir),
    agentDir: realpathSync(agentDir),
    pluginDir,
  };
}

export async function cleanupPiIsolatedEnv(env: PiIsolatedEnv): Promise<void> {
  rmSync(env.baseDir, { recursive: true, force: true });
}
