import { mkdirSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

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
  const baseDirRaw = join(tmpdir(), unique);
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
