import { mkdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

export function enterWindowsUserConfigFixture(root: string) {
  const env = {
    HOME: "home",
    USERPROFILE: "profile",
    APPDATA: "roaming",
    XDG_CONFIG_HOME: "",
    OPENCODE_CONFIG_DIR: "opencode",
  };
  const saved = Object.keys(env).map((key) => [key, process.env[key]] as const);
  const platform = Object.getOwnPropertyDescriptor(process, "platform")!;
  const cwd = process.cwd();

  // Relative Windows paths stay inside the fixture even on a POSIX host,
  // where the filesystem treats their backslashes as literal characters.
  process.chdir(root);
  Object.assign(process.env, env);
  Object.defineProperty(process, "platform", { configurable: true, value: "win32" });

  return {
    env,
    legacy: "home\\.config\\cortexkit\\aft.jsonc",
    canonical: "roaming\\cortexkit\\aft.jsonc",
    restore() {
      Object.defineProperty(process, "platform", platform);
      for (const [key, value] of saved) {
        if (value === undefined) delete process.env[key];
        else process.env[key] = value;
      }
      process.chdir(cwd);
    },
  };
}

export function writeSettingsFixture(path: string, text: string): void {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, text);
}
