import { OpenCodeAdapter } from "../adapters/opencode.js";
import type { HarnessAdapter } from "../adapters/types.js";
import { CLI } from "../lib/cli.js";
import { resolveAdaptersForCommand } from "../lib/harness-select.js";
import { ensureAftSchemaUrl } from "../lib/jsonc.js";
import { intro, log, note, outro } from "../lib/prompts.js";
import { formatHostGenerations, type OpenCodeHostDetection } from "../setup/host-generation.js";

export interface SetupOptions {
  resolveAdapters?: typeof resolveAdaptersForCommand;
  detectOpenCodeHost?: () => OpenCodeHostDetection;
}

export async function runSetup(argv: string[], options: SetupOptions = {}): Promise<number> {
  intro(`${CLI} setup`);

  const adapters = await (options.resolveAdapters ?? resolveAdaptersForCommand)(argv, {
    allowMulti: true,
    verb: "setup",
  });

  let anyFailure = false;
  for (const adapter of adapters) {
    log.info(`${adapter.displayName}: configuring ${adapter.pluginPackageName}…`);
    if (!adapter.isInstalled()) {
      log.warn(
        `${adapter.displayName} host not found on PATH. ${adapter.getInstallHint()} and rerun \`${CLI} setup\`.`,
      );
      anyFailure = true;
      continue;
    }

    if (adapter instanceof OpenCodeAdapter) {
      const detection = options.detectOpenCodeHost
        ? options.detectOpenCodeHost()
        : adapter.detectHostGeneration();
      log.info(`${adapter.displayName}: host generation ${formatHostGenerations(detection)}`);
      if (detection.status === "ambiguous") {
        log.error(
          `${adapter.displayName}: both V1 and V2 hosts are installed; refusing to change either config until only one generation is selected on PATH.`,
        );
        anyFailure = true;
        continue;
      }
      if (detection.status === "unknown") {
        log.warn(
          `${adapter.displayName}: host generation is unavailable (Desktop-only installs may not expose package metadata); applying the generation-independent exact plugin pin.`,
        );
      }
    }

    const result = await adapter.ensurePluginEntry();
    if (!result.ok) {
      log.error(`${adapter.displayName}: ${result.message}`);
      anyFailure = true;
      continue;
    }

    switch (result.action) {
      case "already_present":
        log.success(`${adapter.displayName}: already set up (${result.configPath})`);
        break;
      case "added":
      case "updated":
        log.success(`${adapter.displayName}: ${result.message}`);
        break;
      default:
        log.info(`${adapter.displayName}: ${result.message}`);
    }

    // OpenCode's TUI sidebar plugin lives in tui.json(c). Registered here (and
    // in doctor --fix) ONLY — the runtime plugin never injects it, so a user
    // who removes the entry stays removed across launches.
    if (adapter.ensureTuiPluginEntry) {
      const tuiResult = await adapter.ensureTuiPluginEntry();
      if (!tuiResult.ok) {
        log.warn(`${adapter.displayName}: ${tuiResult.message}`);
      } else if (tuiResult.action === "added" || tuiResult.action === "updated") {
        log.success(`${adapter.displayName}: ${tuiResult.message}`);
      }
    }

    // Ensure aft.jsonc has $schema pointing at the generated JSON Schema so
    // editors get autocomplete + validation for AFT config fields.
    try {
      const { aftConfig, aftConfigFormat } = adapter.detectConfigPaths();
      const schemaResult = ensureAftSchemaUrl(aftConfig, aftConfigFormat);
      if (schemaResult.action === "added" || schemaResult.action === "updated") {
        log.success(`${adapter.displayName}: ${schemaResult.message}`);
      }
    } catch (error) {
      log.warn(
        `${adapter.displayName}: could not set $schema on aft.jsonc: ${
          error instanceof Error ? error.message : String(error)
        }`,
      );
    }

    printNextSteps(adapter);
  }

  if (anyFailure) {
    outro("Setup finished with warnings — see above.");
    return 1;
  }
  outro("Done.");
  return 0;
}

function printNextSteps(adapter: HarnessAdapter): void {
  if (adapter.kind === "opencode") {
    note(
      [
        "Restart OpenCode (or reload your session) so the plugin loads.",
        `Verify with: \`${CLI} doctor\`.`,
      ].join("\n"),
      "Next steps",
    );
    return;
  }
  if (adapter.kind === "pi") {
    note(
      [
        "Restart your Pi session so the extension registers.",
        `Verify with: \`${CLI} doctor\`.`,
      ].join("\n"),
      "Next steps",
    );
  }
}
