import { homedir } from "node:os";
import { join, resolve } from "node:path";

export type OpenCodeBinaryType = "opencode" | "opencode-desktop";

export interface OpenCodeConfigDirOptions {
  binary: OpenCodeBinaryType;
  version?: string | null;
  checkExisting?: boolean;
}

export interface OpenCodeConfigPaths {
  configDir: string;
  configJson: string;
  configJsonc: string;
  packageJson: string;
  omoConfig: string;
}

function getCliConfigDir(): string {
  const envConfigDir = process.env.OPENCODE_CONFIG_DIR?.trim();
  if (envConfigDir) {
    return resolve(envConfigDir);
  }

  if (process.platform === "win32") {
    return join(homedir(), ".config", "opencode");
  }

  return join(process.env.XDG_CONFIG_HOME || join(homedir(), ".config"), "opencode");
}

export function getOpenCodeConfigDir(_options: OpenCodeConfigDirOptions): string {
  return getCliConfigDir();
}

export function getOpenCodeConfigPaths(options: OpenCodeConfigDirOptions): OpenCodeConfigPaths {
  const configDir = getOpenCodeConfigDir(options);
  return {
    configDir,
    configJson: join(configDir, "opencode.json"),
    configJsonc: join(configDir, "opencode.jsonc"),
    packageJson: join(configDir, "package.json"),
    omoConfig: join(configDir, "magic-context.jsonc"),
  };
}
