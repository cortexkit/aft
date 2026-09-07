import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, isAbsolute, join, resolve } from "node:path";
import type { MigrationHarness } from "./migration.js";

export interface ResolvedAftConfigPaths {
  userConfigPath: string;
  projectConfigPath: string;
}

export interface LegacyAftConfigSource {
  path: string;
  label: string;
  /** Which harness this legacy config belonged to (drives operating-harness-wins). */
  harness?: MigrationHarness;
}

function homeDir(): string {
  if (process.platform === "win32") return process.env.USERPROFILE || process.env.HOME || homedir();
  return process.env.HOME || homedir();
}

// This copy becomes the daemon's config-home callable when exported. The call
// replaces it with: non-empty XDG_CONFIG_HOME; Windows-only non-empty APPDATA;
// Windows-only non-empty USERPROFILE plus `AppData/Roaming`; non-empty HOME plus
// `.config`; relative `.config`. Last re-derived 2026-09-06 against subconscious
// d5e09914b0791a66f2a5a00a9bb3422860ade95e: compare ordered guards with
// `subc-core/src/daemon_config.rs::default_config_path` and resolve
// `DAEMON_CONFIG_RELATIVE_PATH` before editing this temporary copy.
function configHome(): string {
  const xdg = process.env.XDG_CONFIG_HOME;
  if (xdg && isAbsolute(xdg)) return xdg;
  return join(homeDir(), ".config");
}

function legacyOpenCodeConfigDir(): string {
  const envDir = process.env.OPENCODE_CONFIG_DIR?.trim();
  if (envDir) return resolve(envDir);
  return join(configHome(), "opencode");
}

function legacyPiAgentDir(): string {
  return join(homeDir(), ".pi", "agent");
}

function legacySources(
  basePath: string,
  label: string,
  harness: MigrationHarness,
): LegacyAftConfigSource[] {
  return [
    { path: `${basePath}.jsonc`, label: `${label} aft.jsonc`, harness },
    { path: `${basePath}.json`, label: `${label} aft.json`, harness },
  ];
}

export function resolveCortexKitUserConfigPath(): string {
  return join(configHome(), "cortexkit", "aft.jsonc");
}

export function resolveCortexKitProjectConfigPath(projectDirectory: string): string {
  return join(projectDirectory, ".cortexkit", "aft.jsonc");
}

export function resolveCortexKitConfigPaths(projectDirectory: string): ResolvedAftConfigPaths {
  return {
    userConfigPath: resolveCortexKitUserConfigPath(),
    projectConfigPath: resolveCortexKitProjectConfigPath(projectDirectory),
  };
}

export function resolveLegacyAftConfigSources(projectDirectory: string): {
  user: LegacyAftConfigSource[];
  project: LegacyAftConfigSource[];
} {
  return {
    user: [
      ...legacySources(join(legacyOpenCodeConfigDir(), "aft"), "OpenCode user", "opencode"),
      ...legacySources(join(legacyPiAgentDir(), "aft"), "Pi user", "pi"),
    ],
    project: [
      ...legacySources(join(projectDirectory, ".opencode", "aft"), "OpenCode project", "opencode"),
      ...legacySources(join(projectDirectory, ".pi", "aft"), "Pi project", "pi"),
    ],
  };
}

export function resolveHarnessStoragePath(
  storageRoot: string,
  harness: MigrationHarness,
  ...segments: string[]
): string {
  return join(storageRoot, harness, ...segments);
}

export function repairRootScopedStorageFile(
  storageRoot: string,
  harness: MigrationHarness,
  fileName: string,
): string {
  const harnessPath = resolveHarnessStoragePath(storageRoot, harness, fileName);
  const rootPath = join(storageRoot, fileName);

  if (existsSync(harnessPath) || !existsSync(rootPath)) return harnessPath;

  try {
    mkdirSync(dirname(harnessPath), { recursive: true });
    renameSync(rootPath, harnessPath);
  } catch {
    // Best-effort compatibility repair. Callers still use the harness path so
    // new writes stop extending the root-scoped layout.
  }

  return harnessPath;
}

/**
 * Decides whether to surface the version-specific announcement dialog/toast.
 *
 * Three cases, all driven off the persisted `last_announced_version` file:
 *
 * 1. **Existing user, same version** — file matches `currentVersion`. Skip.
 *
 * 2. **Existing user, upgrade** — file holds a *different* non-empty version.
 *    Show the dialog so the user sees what's new in their upgrade. After the
 *    dialog is dismissed, the host calls `markAnnouncementSeen` to record
 *    `currentVersion`.
 *
 * 3. **Fresh install or ephemeral sandbox** — file does not exist OR holds
 *    only whitespace. We deliberately do NOT show changelog bullets to a
 *    first-time user (no context to interpret them), AND we don't pester
 *    Docker/CI/disposable-VM users whose storage gets wiped every boot.
 *    Instead we silently **seed** the file with `currentVersion` so the very
 *    next launch behaves like case 1. Future upgrades still trigger case 2.
 *
 * Failures to read/write the marker file are non-fatal: we never let a
 * filesystem hiccup spam an announcement. On any I/O error the function
 * returns `false` and the host treats this turn as already-announced.
 *
 * Returns:
 *   - `true`  → the caller should render the announcement and then call
 *               `markAnnouncementSeen(...)` once the user has seen it.
 *   - `false` → skip rendering. (File was already up-to-date, OR this was a
 *               fresh-install seed and the file has now been written so the
 *               next launch will also skip.)
 */
export function shouldShowAnnouncement(
  storageRoot: string,
  harness: MigrationHarness,
  currentVersion: string,
): boolean {
  if (!currentVersion) return false;

  const versionFile = repairRootScopedStorageFile(storageRoot, harness, "last_announced_version");

  let lastVersion = "";
  try {
    if (existsSync(versionFile)) {
      lastVersion = readFileSync(versionFile, "utf-8").trim();
    }
  } catch {
    // Read failed — be conservative and skip the announcement so a flaky
    // filesystem can't repeatedly flash a dialog.
    return false;
  }

  if (lastVersion === currentVersion) return false;

  if (!lastVersion) {
    // Fresh install or sandbox: silently mark as seen. The next launch sees
    // case 1 (file matches) and stays quiet. Real upgrades from a persisted
    // older version still hit the `lastVersion !== currentVersion` path
    // above and surface the dialog.
    try {
      mkdirSync(dirname(versionFile), { recursive: true });
      writeFileSync(versionFile, currentVersion);
    } catch {
      // Best-effort. If we couldn't seed the file we still skip this turn so
      // the user isn't pestered; we'll just try to seed again next launch.
    }
    return false;
  }

  return true;
}

/**
 * Records that the user has seen `currentVersion`'s announcement. Best-effort
 * filesystem write — failures are silently swallowed because the worst case
 * is repeating the announcement once, not a broken plugin.
 */
export function markAnnouncementSeen(
  storageRoot: string,
  harness: MigrationHarness,
  currentVersion: string,
): void {
  if (!currentVersion) return;

  const versionFile = repairRootScopedStorageFile(storageRoot, harness, "last_announced_version");

  try {
    mkdirSync(dirname(versionFile), { recursive: true });
    writeFileSync(versionFile, currentVersion);
  } catch {
    // Best-effort.
  }
}

/**
 * Decode an RFC 8089 `file:` URL to a local path. Models routinely spell
 * local targets as file:///path (or file:/path, file://localhost/path);
 * rejecting them only produces failed tool calls. MUST match the Rust
 * decoder (subc_translate.rs decode_file_url) so plugin-side permission
 * gates judge the same target the server resolves. Returns the input
 * unchanged when it is not a decodable local file URL.
 *
 * The percent-decode is byte-wise and tolerant, matching Rust EXACTLY:
 * each valid %HH decodes independently; malformed escapes stay literal.
 * decodeURIComponent is all-or-nothing (throws on one malformed %ZZ,
 * leaving VALID escapes like %2e%2e encoded too) — that divergence lets a
 * crafted URL read as in-project at the permission gate while the server
 * resolves the decoded ../ out of the project.
 */
export function decodeFileUrl(target: string): string {
  if (!target.startsWith("file:")) return target;
  const rest = target.slice("file:".length);
  let pathPart: string;
  if (rest.startsWith("//")) {
    const after = rest.slice(2);
    const slash = after.indexOf("/");
    const authority = slash === -1 ? after : after.slice(0, slash);
    const p = slash === -1 ? "" : after.slice(slash);
    if (authority === "" || authority === "localhost") pathPart = p;
    else if (process.platform === "win32") pathPart = `//${authority}${p}`;
    else return target;
  } else if (rest.startsWith("/")) {
    pathPart = rest;
  } else {
    return target;
  }
  const bytes: number[] = [];
  for (let i = 0; i < pathPart.length; ) {
    if (pathPart[i] === "%" && i + 2 < pathPart.length) {
      const hex = pathPart.slice(i + 1, i + 3);
      if (/^[0-9a-fA-F]{2}$/.test(hex)) {
        bytes.push(Number.parseInt(hex, 16));
        i += 3;
        continue;
      }
    }
    const cp = pathPart.codePointAt(i) as number;
    const ch = String.fromCodePoint(cp);
    for (const b of Buffer.from(ch, "utf8")) bytes.push(b);
    i += ch.length;
  }
  const decoded = Buffer.from(bytes).toString("utf8");
  // file:///C:/x decodes to /C:/x — strip the slash for the drive form.
  if (process.platform === "win32" && /^\/[A-Za-z]:/.test(decoded)) {
    return decoded.slice(1);
  }
  return decoded;
}
