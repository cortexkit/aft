import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";

export interface OmpPaths {
  configRoot: string;
  agentDir: string;
  dataRoot: string;
  dataAgentRoot: string;
  pluginsDir: string;
  sessionsRoot: string;
}

function envFirstHomeDir(): string {
  return process.env.HOME?.trim() || homedir();
}

/** Mirror OMP's profile, custom agent-directory, and XDG data resolution. */
export function resolveOmpPaths(): OmpPaths {
  const normalizedProfile = (process.env.OMP_PROFILE ?? process.env.PI_PROFILE)?.trim();
  const profile =
    normalizedProfile &&
    normalizedProfile !== "default" &&
    /^[a-z0-9][a-z0-9._-]{0,63}$/.test(normalizedProfile)
      ? normalizedProfile
      : undefined;
  const configDirName = process.env.PI_CONFIG_DIR?.trim() || ".omp";
  const baseConfigRoot = join(envFirstHomeDir(), configDirName);
  const configRoot = profile ? join(baseConfigRoot, "profiles", profile) : baseConfigRoot;
  const defaultAgentDir = join(configRoot, "agent");

  // A named profile owns its complete layout, so a stale default-profile
  // PI_CODING_AGENT_DIR must not move profile data outside the profile root.
  const configuredAgentDir = profile ? undefined : process.env.PI_CODING_AGENT_DIR?.trim();
  const agentDir = configuredAgentDir ? resolve(configuredAgentDir) : defaultAgentDir;
  const canUseXdg =
    (process.platform === "linux" || process.platform === "darwin") && agentDir === defaultAgentDir;

  let dataRoot = configRoot;
  if (canUseXdg) {
    const xdgDataHome = process.env.XDG_DATA_HOME?.trim();
    if (xdgDataHome) {
      const appRoot = join(xdgDataHome, "omp");
      const candidate = profile ? join(appRoot, "profiles", profile) : appRoot;
      if (existsSync(candidate)) dataRoot = candidate;
    }
  }

  // OMP drops the agent/ segment when an initialized XDG data root is active.
  const dataAgentRoot = dataRoot === configRoot ? agentDir : dataRoot;
  return {
    configRoot,
    agentDir,
    dataRoot,
    dataAgentRoot,
    pluginsDir: join(dataRoot, "plugins"),
    sessionsRoot: join(dataAgentRoot, "sessions"),
  };
}

export function getOmpAgentDir(): string {
  return resolveOmpPaths().agentDir;
}

export function getOmpPluginsDir(): string {
  return resolveOmpPaths().pluginsDir;
}

export function getOmpPluginsLockPath(): string {
  return join(getOmpPluginsDir(), "omp-plugins.lock.json");
}

export function getOmpSessionsRoot(): string {
  return resolveOmpPaths().sessionsRoot;
}

/** Resolve OMP's package-root override used by Nix, Guix, and source installs. */
export function getOmpPackageDir(): string | undefined {
  const value = process.env.PI_PACKAGE_DIR?.trim();
  if (!value) return undefined;
  if (value === "~") return envFirstHomeDir();
  if (value.startsWith("~/") || value.startsWith("~\\")) {
    return resolve(envFirstHomeDir(), value.slice(2));
  }
  return resolve(value);
}
