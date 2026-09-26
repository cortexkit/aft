/**
 * Orchestrates AFT's npm-based LSP auto-install.
 *
 * Flow at plugin startup:
 *
 *   1. Resolve already-cached LSP binary directories. Pass these to Rust as
 *      `lsp_paths_extra` so the layered resolver finds them before PATH.
 *
 *   2. For each Pattern B/D server whose project is "relevant" (root marker
 *      OR matching extension exists in the bounded project walk) AND not yet cached
 *      AND not in `lsp.disabled`:
 *
 *        a. If a user pinned a version via `lsp.versions: {"<pkg>": "X"}`,
 *           use that version directly (skip 7-day grace).
 *        b. Else if cache says we checked recently (< grace_days ago) and
 *           we have a known eligible version, use it.
 *        c. Else probe the npm registry, apply 7-day grace filter.
 *        d. If grace blocks all candidates AND the package is already
 *           installed, log a warning and keep the existing version.
 *           Otherwise skip + warn.
 *
 *   3. Spawn `npm install --no-save <pkg>@<version> --ignore-scripts`
 *      in the background. Drop a lockfile while running. Log progress.
 *
 *   4. When the background install settles, the plugin re-scans the cache and
 *      reconfigures any live bridge without restarting it. Future bridges use
 *      the same paths through the pool's configure overrides.
 */

import { createHash } from "node:crypto";
import {
  createReadStream,
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import {
  type AftTransportPool,
  npmInvocation,
  npmSpawnEnv,
  resolveNpm,
  spawn,
  terminateNpmProcessTree,
} from "@cortexkit/aft-bridge";
import { error, log, warn } from "./logger.js";
import {
  type InstallLockLease,
  isInstalled,
  lspBinaryPath,
  lspBinDir,
  lspPackageDir,
  readInstalledMeta,
  readVersionCheck,
  shouldRecheckVersion,
  withInstallLock,
  writeInstalledMeta,
  writeVersionCheck,
} from "./lsp-cache.js";
import { assertSafeVersion, isSafeVersion } from "./lsp-github-probe.js";
import { NPM_LSP_TABLE, type NpmServerSpec } from "./lsp-npm-table.js";
import {
  hasPackageJsonDep,
  hasRootMarker,
  relevantExtensionsInProject,
} from "./lsp-project-relevance.js";
import { probeRegistry, type VersionPickResult } from "./lsp-registry-probe.js";

/** Per-call configuration drawn from `lsp.*` plugin config. */
export interface AutoInstallConfig {
  /** Master enable. Default: true. */
  autoInstall: boolean;
  /** Supply-chain grace window. Default: 7. */
  graceDays: number;
  /** User-pinned versions (bypasses grace). E.g. `{ "pyright": "1.1.300" }`. */
  versions: Readonly<Record<string, string>>;
  /** Server IDs the user explicitly disabled. Lowercase string match against `NpmServerSpec.id`. */
  disabled: ReadonlySet<string>;
}

/** Result returned to the caller. */
export interface AutoInstallResult {
  /** Bin directories of every cached install — pass to Rust as `lsp_paths_extra`. */
  cachedBinDirs: string[];
  /** Number of background installs kicked off. */
  installsStarted: number;
  /** Binary names whose installs are actively in flight at return time. */
  installingBinaries: string[];
  /**
   * Servers that were disabled or skipped at decision time (synchronous).
   *
   * Note: this only includes synchronous reasons (disabled, irrelevant,
   * `auto_install: false`). Async reasons (grace blocked, registry probe
   * failed, install crashed) populate via the `installsComplete` callback
   * because they're known only after the background work runs.
   */
  skipped: Array<{ id: string; reason: string }>;
  /**
   * Promise that resolves when EVERY backgrounded install settles. Each
   * install holds its per-package install lock for the entire duration;
   * concurrent sessions racing into the same package will see the lock
   * held and back off honestly.
   *
   * Plugin startup ignores this; tests await it to assert install outcomes.
   * Each completed install pushes its skip-reason into `skipped` (mutates
   * the array shared with the synchronous return value).
   */
  installsComplete: Promise<void>;
  /** Re-scan the cache after background installs settle. */
  getCachedBinDirs: () => string[];
}

/**
 * Cheap project-relevance check. A server is relevant if ANY of:
 *
 *   1. A `rootMarkers` file exists at the project root (e.g. `tsconfig.json`,
 *      `vue.config.ts`, `astro.config.mjs`).
 *   2. `packageJsonDeps` matches an entry in `<root>/package.json` deps maps
 *      — covers Vite/Nuxt/SvelteKit setups where no framework-specific
 *      config file exists at the root (GitHub issue #48).
 *   3. The bounded extension walk found one of `spec.extensions` in the
 *      project tree (depth ≤ 4, ≤ 200 dirs visited).
 *
 * Checks are ordered cheapest-first: `existsSync`, package.json read, then
 * the walk. The walk result is cached across servers within one runAutoInstall
 * call via `projectExtensions`.
 */
function isProjectRelevant(
  spec: NpmServerSpec,
  projectRoot: string,
  projectExtensions: () => ReadonlySet<string>,
): boolean {
  if (hasRootMarker(projectRoot, spec.rootMarkers)) return true;
  if (hasPackageJsonDep(projectRoot, spec.packageJsonDeps)) return true;
  const extensions = projectExtensions();
  return spec.extensions.some((ext) => extensions.has(ext.toLowerCase()));
}

const npmExtToServerIds = buildExtensionMap(NPM_LSP_TABLE);

function buildExtensionMap(specs: readonly NpmServerSpec[]): Record<string, string[]> {
  const byExt: Record<string, string[]> = {};
  for (const spec of specs) {
    for (const ext of spec.extensions) {
      const key = ext.toLowerCase();
      byExt[key] ??= [];
      byExt[key].push(spec.id);
    }
  }
  return byExt;
}

interface InFlightAutoInstall {
  controller: AbortController;
  promise: Promise<void>;
}

const inFlightAutoInstalls = new Set<InFlightAutoInstall>();
// A taskkill failure leaves descendant liveness unknowable. Block retries for
// this host session without creating a permanent cross-process tombstone.
const quarantinedNpmInstalls = new Set<string>();

function trackInFlightAutoInstall(
  controller: AbortController,
  promise: Promise<void>,
): Promise<void> {
  const entry: InFlightAutoInstall = { controller, promise };
  inFlightAutoInstalls.add(entry);
  promise.then(
    () => inFlightAutoInstalls.delete(entry),
    () => inFlightAutoInstalls.delete(entry),
  );
  return promise;
}

export async function abortInFlightAutoInstalls(): Promise<void> {
  const installs = Array.from(inFlightAutoInstalls);
  for (const install of installs) {
    install.controller.abort();
  }
  await Promise.allSettled(installs.map((install) => install.promise));
}

/**
 * Resolve which version to install for `spec`, honoring user pins,
 * cached version checks, and the 7-day grace.
 *
 * Returns the version string to install, or null when:
 *   - probe failed and nothing is cached
 *   - all candidates are within grace (blockedByGrace)
 *
 * `null` callers should fall back to whatever's installed (if anything),
 * else skip + warn.
 */
async function resolveTargetVersion(
  spec: NpmServerSpec,
  config: AutoInstallConfig,
  fetchImpl: typeof fetch = fetch,
): Promise<{ version: string | null; pinned: boolean; probe: VersionPickResult | null }> {
  // 1. User pin.
  // Validate npm pins with the same regex GitHub pins use.
  // Bun/npm version specifiers also accept `file:`, `npm:`, `git+https:` schemes
  // that could redirect the install to an attacker-controlled source if the user
  // config is compromised. Even though spawn() with argv array prevents shell
  // injection, the version string is interpreted by bun, not the shell.
  const pinned = config.versions[spec.npm];
  if (pinned) {
    assertSafeVersion(pinned);
    return { version: pinned, pinned: true, probe: null };
  }

  // 2. Cached check still fresh.
  //
  // Validate cached.latest_eligible before consuming.
  // Disk corruption or future bug could put unsafe value here. Treat
  // unsafe cache as miss so the next branch forces a fresh probe.
  const cached = readVersionCheck(spec.npm);
  const weeklyMs = config.graceDays * 24 * 60 * 60 * 1000;
  const cachedSafe = isSafeVersion(cached?.latest_eligible ?? null);
  if (cached && !shouldRecheckVersion(cached, weeklyMs) && cachedSafe) {
    return { version: cached.latest_eligible as string, pinned: false, probe: null };
  }

  // 3. Probe the registry.
  const probe = await probeRegistry(spec.npm, config.graceDays, fetchImpl);
  if (!probe) {
    // Probe failed entirely — fall back to cached if any (and only if safe).
    return {
      version: cachedSafe ? (cached?.latest_eligible ?? null) : null,
      pinned: false,
      probe: null,
    };
  }

  writeVersionCheck(spec.npm, probe.version);
  return { version: probe.version, pinned: false, probe };
}

/**
 * Spawn `npm install --no-save <pkg>@<version>` in the cache dir.
 *
 * Uses `--ignore-scripts` to neutralize lifecycle hooks (the v0.16 audit
 * hardening). Output goes to plugin log.
 */
/**
 * Anchor an `npm install --no-save` to `cwd` by writing a minimal package.json.
 *
 * Without a package.json in `cwd`, npm walks UP the directory tree; if any
 * ancestor (e.g. ~/package.json) has one, npm installs into THAT package's
 * node_modules instead, leaving our cache dir's node_modules/<server> missing
 * while still exiting 0. The result is a silent install failure and a recurring
 * lsp_binary_missing warning. The old `bun add` flow created this package.json
 * implicitly; npm needs it written explicitly. GitHub #92.
 *
 * Idempotent: only writes when absent. Failures are non-fatal (logged).
 */
export function ensureInstallAnchor(cwd: string): void {
  try {
    const stub = join(cwd, "package.json");
    if (!existsSync(stub)) {
      writeFileSync(
        stub,
        `${JSON.stringify({ name: "aft-lsp-cache", version: "0.0.0", private: true })}\n`,
      );
    }
  } catch (err) {
    warn(`[lsp] could not write package.json stub in ${cwd}: ${err}`);
  }
}

function runInstall(
  spec: NpmServerSpec,
  version: string,
  cwd: string,
  installLock: InstallLockLease,
  signal?: AbortSignal,
): Promise<boolean> {
  return new Promise((resolve) => {
    const target = `${spec.npm}@${version}`;
    log(`[lsp] installing ${target} to ${cwd}`);

    if (signal?.aborted) {
      warn(`[lsp] install ${target} aborted before spawn`);
      resolve(false);
      return;
    }

    // Resolve npm beyond PATH. Windows npm is `npm.cmd` (Node's spawn does not
    // auto-resolve `.cmd`), and GUI/Desktop launches often have a stripped PATH
    // with no version-manager bin dir, so a bare `npm` spawn fails with ENOENT.
    // resolveNpm() handles discovery, npmSpawnEnv() makes npm's node sibling
    // reachable, and npmInvocation() safely routes Windows shims through cmd.exe
    // without enabling shell parsing on other platforms.
    const npm = resolveNpm();
    if (!npm) {
      warn(`[lsp] npm not found on PATH or known locations; cannot install ${target}`);
      resolve(false);
      return;
    }

    ensureInstallAnchor(cwd);

    const invocation = npmInvocation(npm, [
      "install",
      "--no-save",
      "--ignore-scripts",
      "--silent",
      target,
    ]);
    const child = spawn(invocation.command, invocation.args, {
      stdio: ["ignore", "pipe", "pipe"],
      cwd,
      env: { ...npmSpawnEnv(npm), ...invocation.env },
      windowsVerbatimArguments: invocation.windowsVerbatimArguments,
    });
    child.unref();

    let stderrBuf = "";
    let settled = false;
    let terminationPromise: Promise<void> | null = null;

    const cleanup = () => {
      signal?.removeEventListener("abort", onAbort);
    };
    const finish = (ok: boolean) => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve(ok);
    };
    const childCompletedSuccessfully = () => child.exitCode === 0 && child.signalCode === null;
    const finishAfterConfirmedTermination = () => {
      const completedSuccessfully = childCompletedSuccessfully();
      if (completedSuccessfully) log(`[lsp] installed ${target}`);
      finish(completedSuccessfully);
    };
    const onAbort = () => {
      if (childCompletedSuccessfully()) {
        finishAfterConfirmedTermination();
        return;
      }
      warn(`[lsp] install ${target} aborted during shutdown`);
      terminationPromise ??= terminateNpmProcessTree(child, invocation);
      void terminationPromise.then(finishAfterConfirmedTermination, (terminationError) => {
        // A child with no PID never spawned, so it cannot still be mutating the
        // cache and does not need cross-process quarantine.
        if (child.pid === undefined) {
          error(`[lsp] install ${target} failed to spawn during shutdown: ${terminationError}`);
          finish(false);
          return;
        }
        quarantinedNpmInstalls.add(spec.npm);
        installLock.retain();
        error(
          `[lsp] install ${target} termination outcome unknown; quarantining retries for this session: ${String(terminationError)}`,
        );
        finish(false);
      });
    };

    signal?.addEventListener("abort", onAbort, { once: true });
    if (signal?.aborted) onAbort();
    child.stdout?.on("data", () => {
      // Suppress stdout — npm chatter is noisy.
    });
    child.stderr?.on("data", (chunk) => {
      const text = String(chunk);
      stderrBuf += text;
      if (stderrBuf.length > 4096) {
        stderrBuf = stderrBuf.slice(stderrBuf.length - 4096);
      }
    });
    child.on("error", (err) => {
      if (settled) return;
      error(`[lsp] install ${target} child-process error: ${err}`);
      // Once abort termination owns settlement, a post-spawn error must not
      // release the install lock before the terminator confirms process exit.
      if (terminationPromise && child.pid !== undefined) return;
      finish(false);
    });
    child.on("exit", (code) => {
      // Abort settlement is owned by the awaited terminator on every platform;
      // do not report a raced wrapper exit independently.
      if (settled || terminationPromise) return;
      if (code === 0) {
        log(`[lsp] installed ${target}`);
        finish(true);
      } else {
        error(
          `[lsp] install ${target} exited with code ${code}; last stderr:\n${stderrBuf.trim()}`,
        );
        finish(false);
      }
    });
  });
}

async function ensureServerInstalled(
  spec: NpmServerSpec,
  config: AutoInstallConfig,
  fetchImpl: typeof fetch,
  signal?: AbortSignal,
): Promise<{ started: boolean; reason?: string }> {
  if (quarantinedNpmInstalls.has(spec.npm)) {
    return {
      started: false,
      reason: "install blocked after unconfirmed npm termination; restart the host to retry",
    };
  }

  // The lock MUST be held through install completion, not just through the
  // start decision. Two parallel sessions would otherwise both pass the
  // "is install needed" check and run `bun add` into the same cache dir
  // concurrently, corrupting node_modules.
  //
  // We hold the lock for the full install promise via withInstallLock(). The
  // install itself is fired-and-forgotten from the caller's perspective —
  // runAutoInstall awaits ensureServerInstalled which awaits withInstallLock
  // which awaits the install — but the OUTER call site in index.ts uses
  // .catch on the whole runAutoInstall promise, so the plugin doesn't block
  // on it. The lock is still released when each install actually finishes.
  const outcome = await withInstallLock(spec.npm, async (installLock) => {
    const { version, probe } = await resolveTargetVersion(spec, config, fetchImpl);

    // Grace blocked + nothing installed = skip with warning.
    if (!version) {
      const installed = isInstalled(spec.npm, spec.binary);
      if (installed) {
        warn(
          `[lsp] no eligible version of ${spec.npm} (grace=${config.graceDays}d); keeping existing install`,
        );
        return { started: false, reason: "kept existing install" };
      }
      const blocked = probe?.blockedByGrace
        ? `all versions are within ${config.graceDays}-day grace window`
        : "registry probe failed";
      warn(`[lsp] skipping ${spec.npm}: ${blocked}`);
      return { started: false, reason: blocked };
    }

    // Skip-if-installed compares installed version vs target.
    //
    // When the same version is already installed AND we
    // recorded a sha256 last time, verify the binary still hashes to the
    // same value. A mismatch is a TOFU violation — refuse the install.
    if (isInstalled(spec.npm, spec.binary)) {
      const installedMeta = readInstalledMeta(spec.npm);
      if (installedMeta && installedMeta.version === version) {
        if (installedMeta.sha256) {
          const currentHash = await hashInstalledBinary(spec).catch((err: unknown) => {
            warn(`[lsp] could not hash existing ${spec.npm} binary for TOFU check: ${err}`);
            return null;
          });
          if (currentHash && currentHash !== installedMeta.sha256) {
            error(
              `[lsp] ${spec.npm}@${version}: TOFU sha256 mismatch — refusing to use ` +
                `tampered binary. Recorded ${installedMeta.sha256}, current ${currentHash}. ` +
                `Run \`npx @cortexkit/aft doctor --clear\` to re-install from scratch.`,
            );
            return {
              started: false,
              reason: `TOFU sha256 mismatch on ${spec.npm}@${version} — see plugin log`,
            };
          }
        }
        return { started: false, reason: "already installed" };
      }
      if (installedMeta) {
        log(`[lsp] reinstalling ${spec.npm}: cached ${installedMeta.version} ≠ target ${version}`);
      } else {
        log(`[lsp] reinstalling ${spec.npm}@${version}: no installed-version metadata recorded`);
      }
    }

    // Run the install AND wait for completion before releasing the lock.
    // Errors are logged but we still return { started: true } so the caller
    // counts the attempt. The next session will retry if installation failed.
    const ok = await runInstall(
      spec,
      version,
      cachedPackageDir(spec.npm),
      installLock,
      signal,
    ).catch((err: unknown) => {
      error(`[lsp] background install ${spec.npm} crashed: ${err}`);
      return false;
    });
    if (!ok) {
      return { started: true, reason: "install failed (see plugin log)" };
    }
    // Record version AND sha256.
    const installedHash = await hashInstalledBinary(spec).catch((err: unknown) => {
      warn(`[lsp] could not hash newly-installed ${spec.npm} binary: ${err}`);
      return null;
    });
    if (installedHash) {
      log(`[lsp] ${spec.npm}@${version} installed sha256=${installedHash}`);
    }
    writeInstalledMeta(spec.npm, version, installedHash ?? undefined);
    return { started: true };
  });

  if (outcome === null) {
    return { started: false, reason: "another install in progress" };
  }
  return outcome;
}

/**
 * Lazy import to avoid a runtime `require` at module top.
 *
 * Returns the directory `<cache>/<encoded-pkg>/` where `bun add` should
 * place `node_modules/`.
 */
function cachedPackageDir(npmPackage: string): string {
  // Reuse the same encoding scheme as lsp-cache.ts.
  // Imported via `import` already through lspBinDir's parent path —
  // dedupe via that helper.
  return lspBinDir(npmPackage).replace(/[\\/]node_modules[\\/]\.bin[\\/]?$/, "");
}

/**
 * Compute the SHA-256 of the installed npm binary.
 *
 * TOFU verification for npm-distributed LSP servers.
 * Streams the binary file directly because npm doesn't have a single
 * archive (bun extracts node_modules across many files).
 */
function hashInstalledBinary(spec: NpmServerSpec): Promise<string> {
  return new Promise((resolve, reject) => {
    const candidates =
      process.platform === "win32"
        ? [
            lspBinaryPath(spec.npm, spec.binary),
            lspBinaryPath(spec.npm, `${spec.binary}.cmd`),
            lspBinaryPath(spec.npm, `${spec.binary}.exe`),
            lspBinaryPath(spec.npm, `${spec.binary}.bat`),
          ]
        : [lspBinaryPath(spec.npm, spec.binary)];

    let pathToHash: string | null = null;
    for (const p of candidates) {
      try {
        if (statSync(p).isFile()) {
          pathToHash = p;
          break;
        }
      } catch {
        // Continue to the next candidate.
      }
    }
    if (!pathToHash) {
      reject(new Error(`installed binary not found at any of: ${candidates.join(", ")}`));
      return;
    }

    const hash = createHash("sha256");
    const stream = createReadStream(pathToHash);
    stream.on("error", reject);
    stream.on("data", (chunk) => hash.update(chunk));
    stream.on("end", () => resolve(hash.digest("hex")));
  });
}

function installedBinaryPath(spec: NpmServerSpec): string | null {
  const candidates =
    process.platform === "win32"
      ? [
          lspBinaryPath(spec.npm, spec.binary),
          lspBinaryPath(spec.npm, `${spec.binary}.cmd`),
          lspBinaryPath(spec.npm, `${spec.binary}.exe`),
          lspBinaryPath(spec.npm, `${spec.binary}.bat`),
        ]
      : [lspBinaryPath(spec.npm, spec.binary)];
  for (const candidate of candidates) {
    try {
      if (statSync(candidate).isFile()) return candidate;
    } catch {}
  }
  return null;
}

function sha256OfFileSync(path: string): string {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function quarantineCachedNpmInstall(spec: NpmServerSpec, reason: string): void {
  const packageDir = lspPackageDir(spec.npm);
  const dest = join(packageDir, "..", ".quarantine", encodeURIComponent(spec.npm), `${Date.now()}`);
  warn(`[lsp] tofu_mismatch ${spec.npm}: ${reason}; quarantining ${packageDir} -> ${dest}`);
  try {
    mkdirSync(join(dest, ".."), { recursive: true });
    rmSync(dest, { recursive: true, force: true });
    renameSync(packageDir, dest);
  } catch (err) {
    warn(`[lsp] tofu_mismatch ${spec.npm}: failed to quarantine cache entry: ${err}`);
  }
}

function validateCachedNpmInstall(spec: NpmServerSpec): boolean {
  const meta = readInstalledMeta(spec.npm);
  const binaryPath = installedBinaryPath(spec);
  if (!meta?.sha256 || !meta.version || !isSafeVersion(meta.version) || !binaryPath) {
    quarantineCachedNpmInstall(spec, "missing/unsafe metadata or binary");
    return false;
  }
  const currentHash = sha256OfFileSync(binaryPath);
  if (currentHash !== meta.sha256) {
    quarantineCachedNpmInstall(spec, `recorded ${meta.sha256}, current ${currentHash}`);
    return false;
  }
  return true;
}

/**
 * Top-level entry point. Returns the list of bin directories that already
 * have an installed binary AND kicks off background installs for missing
 * packages relevant to this project.
 *
 * Caller passes `cachedBinDirs` to Rust as `lsp_paths_extra`. When a background
 * install settles, the plugin calls `getCachedBinDirs()` to refresh the list and
 * reconfigures live bridges; future bridges use the pool override.
 */
export function runAutoInstall(
  projectRoot: string,
  config: AutoInstallConfig,
  fetchImpl: typeof fetch = fetch,
): AutoInstallResult {
  const cachedBinDirs: string[] = [];
  const skipped: Array<{ id: string; reason: string }> = [];
  const installPromises: Promise<void>[] = [];
  // Start decisions happen asynchronously after grace/probe/lock checks. Keep
  // this synchronous return empty so `lsp_inflight_installs` never suppresses
  // missing-binary warnings for installs that may be skipped before spawning.
  const installingBinaries: string[] = [];
  let installsStarted = 0;
  let projectExtensions: Set<string> | null = null;
  const getProjectExtensions = () => {
    projectExtensions ??= relevantExtensionsInProject(projectRoot, npmExtToServerIds);
    return projectExtensions;
  };

  for (const spec of NPM_LSP_TABLE) {
    // 1. Always include cached bin dirs the Rust resolver can use right now.
    if (isInstalled(spec.npm, spec.binary) && validateCachedNpmInstall(spec)) {
      cachedBinDirs.push(lspBinDir(spec.npm));
    }

    if (config.disabled.has(spec.id)) {
      skipped.push({ id: spec.id, reason: "disabled by config" });
      continue;
    }

    if (!config.autoInstall) {
      // User opted out of auto-install. Cached paths are still surfaced.
      skipped.push({ id: spec.id, reason: "auto_install: false" });
      continue;
    }

    if (!isProjectRelevant(spec, projectRoot, getProjectExtensions)) {
      skipped.push({ id: spec.id, reason: "not relevant to project" });
      continue;
    }

    // Kick off the install asynchronously; do NOT await.
    //
    // The async work holds its per-package lock for its whole duration via
    // ensureServerInstalled() → withInstallLock(). The plugin caller treats
    // the entire auto-install as fire-and-forget — the synchronous return
    // is what blocks plugin startup, and that's just the relevance scan and
    // cached-binary discovery.
    //
    // Tests await `installsComplete` to assert outcomes.
    installsStarted += 1;
    const controller = new AbortController();
    const promise = ensureServerInstalled(spec, config, fetchImpl, controller.signal).then(
      (outcome) => {
        if (!outcome.started) installsStarted -= 1;
        if (outcome.reason && outcome.reason !== "already installed") {
          skipped.push({ id: spec.id, reason: outcome.reason });
        }
      },
      (err) => {
        installsStarted -= 1;
        const reason = err instanceof Error ? err.message : String(err);
        skipped.push({ id: spec.id, reason: `install error: ${reason}` });
        error(`[lsp] background install ${spec.npm} promise rejected: ${reason}`);
      },
    );
    installPromises.push(trackInFlightAutoInstall(controller, promise));
  }

  return {
    cachedBinDirs,
    installingBinaries,
    get installsStarted() {
      return installsStarted;
    },
    skipped,
    installsComplete: Promise.all(installPromises).then(() => {}),
    getCachedBinDirs: () =>
      NPM_LSP_TABLE.filter(
        (spec) => isInstalled(spec.npm, spec.binary) && validateCachedNpmInstall(spec),
      ).map((spec) => lspBinDir(spec.npm)),
  };
}

/** Apply newly discovered cache paths to both future and live bridge config. */
export async function pushLspPathsAfterAutoInstall(
  pool: Pick<AftTransportPool, "setConfigureOverride" | "reconfigure">,
  projectRoot: string,
  cachedBinDirs: readonly string[],
): Promise<void> {
  const paths = [...new Set(cachedBinDirs)];
  pool.setConfigureOverride("lsp_paths_extra", paths);
  await pool.reconfigure(projectRoot, { lsp_paths_extra: paths });
}
