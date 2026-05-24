import type { HarnessAdapter } from "../adapters/types.js";
import { resolveAdaptersForCommand } from "../lib/harness-select.js";
import { ensureAftSchemaUrl } from "../lib/jsonc.js";
import { intro, log, note, outro } from "../lib/prompts.js";

export async function runSetup(argv: string[]): Promise<number> {
  intro("AFT setup");

  const adapters = await resolveAdaptersForCommand(argv, {
    allowMulti: true,
    verb: "setup",
  });

  let anyFailure = false;
  for (const adapter of adapters) {
    log.info(`${adapter.displayName}: configuring ${adapter.pluginPackageName}…`);
    if (!adapter.isInstalled()) {
      log.warn(
        `${adapter.displayName} host not found on PATH. ${adapter.getInstallHint()} and rerun \`aft setup\`.`,
      );
      anyFailure = true;
      continue;
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
        "Verify with: `npx @cortexkit/aft doctor`.",
      ].join("\n"),
      "Next steps",
    );
    return;
  }
  if (adapter.kind === "pi") {
    note(
      [
        "Restart your Pi session so the extension registers.",
        "Verify with: `npx @cortexkit/aft doctor`.",
      ].join("\n"),
      "Next steps",
    );
  }
}
