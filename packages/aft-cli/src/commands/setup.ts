import { OpenCodeAdapter } from "../adapters/opencode.js";
import type { HarnessAdapter } from "../adapters/types.js";
import { type BinaryDownloader, obtainAftBinary } from "../lib/binary-install.js";
import { probeAftBinary } from "../lib/binary-probe.js";
import { CLI } from "../lib/cli.js";
import { installCliLogger } from "../lib/cli-logger.js";
import { nativeRunnerFor } from "../lib/feature-plan.js";
import { formatFsError } from "../lib/fs-errors.js";
import { resolveAdaptersForCommand } from "../lib/harness-select.js";
import { ensureAftSchemaUrl } from "../lib/jsonc.js";
import { intro, log, note, outro } from "../lib/prompts.js";
import { getSelfVersion } from "../lib/self-version.js";
import {
  type FeatureSetupDeps,
  featureMode,
  featureWizardIsInteractive,
  runFeatureSetup,
} from "../setup/feature-wizard.js";
import { describeOpenCodeHost, type OpenCodeHostDetection } from "../setup/host-generation.js";

export interface SetupOptions {
  resolveAdapters?: typeof resolveAdaptersForCommand;
  detectOpenCodeHost?: () => OpenCodeHostDetection;
  /** Overrides for the feature step (native runner, prompts). */
  features?: FeatureSetupDeps;
  /** Path of an installed binary matching this CLI, or null (tests stub it). */
  findBinary?: (version: string) => string | null;
  /** Binary downloader (tests stub it; defaults to the one doctor --fix uses). */
  downloadBinary?: BinaryDownloader;
}

export async function runSetup(argv: string[], options: SetupOptions = {}): Promise<number> {
  // `--plan` prints the binary's plan and nothing else, so stdout stays JSON.
  if (featureMode(argv) === "plan") return runFeatureSetup(argv, options.features);

  installCliLogger({ verbose: argv.includes("--verbose") });
  intro(`${CLI} setup`);

  const adapters = await (options.resolveAdapters ?? resolveAdaptersForCommand)(argv, {
    allowMulti: true,
    verb: "set up",
  });

  let anyFailure = false;
  const nextSteps: HarnessAdapter[] = [];
  for (const adapter of adapters) {
    try {
      const outcome = await configureAdapter(adapter, options);
      if (outcome === "failed") anyFailure = true;
      else nextSteps.push(adapter);
    } catch (error) {
      // A config write that fails in a way the adapter did not anticipate
      // (most often a directory a `sudo` install left owned by root) is
      // reported for this harness only; the remaining steps still run.
      log.error(`${adapter.displayName}: ${formatFsError(error)}`);
      anyFailure = true;
    }
  }

  // Feature choices live in the shared user config, so they are asked once
  // per run rather than once per harness. The wizard runs the native binary,
  // so a clean machine gets it here first, from the same download doctor uses.
  log.step("Features");
  const features = await prepareFeatureStep(argv, options);
  if (features === null) {
    anyFailure = true;
  } else {
    const featureStatus = await runFeatureSetup(argv, features);
    if (featureStatus !== 0) anyFailure = true;
  }

  // Restart instructions come last, after every choice has been saved.
  for (const adapter of nextSteps) printNextSteps(adapter);

  if (anyFailure) {
    outro("Setup finished with warnings — see above.");
    return 1;
  }
  outro("Done.");
  return 0;
}

/** Register the plugin with one harness. Returns "failed" when setup could not finish it. */
async function configureAdapter(
  adapter: HarnessAdapter,
  options: SetupOptions,
): Promise<"ok" | "failed"> {
  log.info(`${adapter.displayName}: configuring ${adapter.pluginPackageName}…`);
  if (!adapter.isInstalled()) {
    log.warn(
      `${adapter.displayName} host not found on PATH. ${adapter.getInstallHint()} and rerun \`${CLI} setup\`.`,
    );
    return "failed";
  }

  if (adapter instanceof OpenCodeAdapter) {
    const detection = options.detectOpenCodeHost
      ? options.detectOpenCodeHost()
      : adapter.detectHostGeneration();
    // The adapter writes the key this generation's host reads, so it has to
    // see the same detection this command reported.
    adapter.useHostDetection(detection);
    if (detection.status === "ambiguous") {
      log.error(
        `${adapter.displayName}: both OpenCode 1 and OpenCode 2 are installed; refusing to change either config until only one of them is on PATH.`,
      );
      return "failed";
    }
    if (detection.status === "unknown") {
      log.warn(
        `${adapter.displayName}: could not tell which OpenCode version is installed (Desktop-only installs may not say); registering the plugin with an exact version pin, which every version reads.`,
      );
    } else {
      log.info(`Found ${describeOpenCodeHost(detection)}.`);
    }
  }

  const result = await adapter.ensurePluginEntry();
  if (!result.ok) {
    log.error(`${adapter.displayName}: ${result.message}`);
    return "failed";
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

  let failed = false;
  // OpenCode's TUI sidebar plugin lives in tui.json(c). Registered here (and
  // in doctor --fix) ONLY — the runtime plugin never injects it, so a user
  // who removes the entry stays removed across launches.
  if (adapter.ensureTuiPluginEntry) {
    const tuiResult = await adapter.ensureTuiPluginEntry();
    if (!tuiResult.ok) {
      log.warn(`${adapter.displayName}: ${tuiResult.message}`);
      failed = true;
    } else if (tuiResult.action === "added" || tuiResult.action === "updated") {
      log.success(`${adapter.displayName}: ${tuiResult.message}`);
    }
  }

  // Ensure aft.jsonc has $schema pointing at the generated JSON Schema so
  // editors get autocomplete + validation for AFT config fields.
  let aftConfigPath: string | undefined;
  try {
    const { aftConfig, aftConfigFormat } = adapter.detectConfigPaths();
    aftConfigPath = aftConfig;
    const schemaResult = ensureAftSchemaUrl(aftConfig, aftConfigFormat);
    if (schemaResult.action === "added" || schemaResult.action === "updated") {
      log.success(`${adapter.displayName}: ${schemaResult.message}`);
    }
  } catch (error) {
    log.warn(
      `${adapter.displayName}: could not set $schema on aft.jsonc: ${formatFsError(error, aftConfigPath)}`,
    );
    failed = true;
  }
  return failed ? "failed" : "ok";
}

/**
 * Make sure the feature step can run: every mode except a non-interactive
 * run without flags calls the native binary, so obtain one matching this CLI
 * when none is installed. Returns the deps for the step, or null when the
 * binary could not be obtained (the cause is already reported).
 */
async function prepareFeatureStep(
  argv: string[],
  options: SetupOptions,
): Promise<FeatureSetupDeps | null> {
  const deps = options.features ?? {};
  if (deps.run) return deps;
  const needsBinary = featureMode(argv) !== "interactive" || featureWizardIsInteractive(deps);
  if (!needsBinary) return deps;

  const version = getSelfVersion();
  const installed = (options.findBinary ?? ((v: string) => probeAftBinary(v).path))(version);
  if (installed) return { ...deps, run: nativeRunnerFor(installed) };

  log.info(`Installing the AFT binary v${version} for this machine…`);
  const obtained = await obtainAftBinary(version, options.downloadBinary);
  if (!obtained.ok) {
    log.error(
      `Could not install the AFT binary, so feature choices were not changed: ${obtained.message}\nAfter fixing that, run \`${CLI} doctor --fix\` to install it, then \`${CLI} setup\` again.`,
    );
    return null;
  }
  log.success(`AFT binary installed at ${obtained.path}`);
  return { ...deps, run: nativeRunnerFor(obtained.path) };
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
  if (adapter.kind === "pi" || adapter.kind === "omp") {
    const host = adapter.kind === "omp" ? "OMP" : "Pi";
    note(
      [
        `Restart your ${host} session so the extension registers.`,
        `Verify with: \`${CLI} doctor\`.`,
      ].join("\n"),
      "Next steps",
    );
  }
}
