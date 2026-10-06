import { mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

// An empty git config file. Git for Windows refuses the `NUL` device as a
// config path ("unable to access 'NUL': Invalid argument"), so Windows uses a
// real empty file; elsewhere `/dev/null` reads as empty.
function hermeticGitConfigPath(): string {
  if (process.platform !== "win32") return "/dev/null";
  const dir = join(tmpdir(), `aft-test-gitconfig-${process.pid}`);
  mkdirSync(dir, { recursive: true });
  const path = join(dir, "empty.gitconfig");
  writeFileSync(path, "");
  return path;
}

const HERMETIC_GIT_CONFIG_PATH = hermeticGitConfigPath();

export const HERMETIC_GIT_CHILD_ENV = {
  GIT_CONFIG_GLOBAL: HERMETIC_GIT_CONFIG_PATH,
  GIT_CONFIG_SYSTEM: HERMETIC_GIT_CONFIG_PATH,
} as const;

export function hermeticGitChildEnv(extra: Record<string, string> = {}): Record<string, string> {
  return {
    ...extra,
    ...HERMETIC_GIT_CHILD_ENV,
  };
}

export function withHermeticGitEnv(
  env: NodeJS.ProcessEnv | Record<string, string | undefined> = process.env,
): NodeJS.ProcessEnv {
  return {
    ...env,
    ...HERMETIC_GIT_CHILD_ENV,
  };
}
