import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { parse, stringify } from "comment-json";
import { log } from "../logger";
import { getOpenCodeConfigPaths } from "./opencode-config-dir";

const PLUGIN_NAME = "@cortexkit/aft-opencode";
const PLUGIN_ENTRY = `${PLUGIN_NAME}@latest`;

function resolveTuiConfigPath(): string {
  const configDir = getOpenCodeConfigPaths({ binary: "opencode" }).configDir;
  const jsoncPath = join(configDir, "tui.jsonc");
  const jsonPath = join(configDir, "tui.json");

  if (existsSync(jsoncPath)) return jsoncPath;
  if (existsSync(jsonPath)) return jsonPath;
  return jsonPath;
}

export function ensureTuiPluginEntry(): boolean {
  try {
    const configPath = resolveTuiConfigPath();

    let config: Record<string, unknown> = {};
    if (existsSync(configPath)) {
      config = (parse(readFileSync(configPath, "utf-8")) as Record<string, unknown>) ?? {};
    }

    const plugins = Array.isArray(config.plugin)
      ? config.plugin.filter((value): value is string => typeof value === "string")
      : [];

    if (
      plugins.some(
        (plugin) =>
          plugin === PLUGIN_NAME ||
          plugin.startsWith(`${PLUGIN_NAME}@`) ||
          plugin.includes("opencode-plugin") ||
          plugin.includes("aft-opencode"),
      )
    ) {
      return false;
    }

    plugins.push(PLUGIN_ENTRY);
    config.plugin = plugins;

    mkdirSync(dirname(configPath), { recursive: true });
    writeFileSync(configPath, `${stringify(config, null, 2)}\n`);
    log(`[aft-plugin] added TUI plugin entry to ${configPath}`);
    return true;
  } catch (error) {
    log(
      `[aft-plugin] failed to update tui.json: ${error instanceof Error ? error.message : String(error)}`,
    );
    return false;
  }
}
