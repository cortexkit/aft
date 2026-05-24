/**
 * GitHub-release auto-installer for LSP servers (Pattern C).
 *
 * Cache layout under `<aft-cache-root>/lsp-binaries/`:
 *
 *   <id>/
 *     bin/<binary>                  ← extracted binary (or shim)
 *     extracted/                    ← validated extraction (kept for upgrades)
 *     .aft-version-check            ← JSON: { last_checked, latest_eligible }
 *     .aft-installing               ← lockfile while a download/extract runs
 *
 * `bin/<binary>` is what we add to `lsp_paths_extra` so the Rust resolver
 * can find it.
 *
 * Extraction containment (audit #3):
 *   1. Download to `<id>/<asset-name>` with a hard size cap (audit #4).
 *   2. Extract into a quarantine dir `<id>/.staging-<rand>/`.
 *   3. Walk the staging tree and reject any entry that is a symlink, hardlink,
 *      or whose canonical path escapes the staging root.
 *   4. Only after validation: `renameSync(staging, extracted)` atomically
 *      replaces any prior extraction.
 *   5. Stage dir is always cleaned up — success or failure.
 *
 * GitHub pin resolution (audit #5):
 *   When `lsp.versions: { "owner/repo": "X" }` is set, we use GitHub's
 *   `/releases/tags/<tag>` endpoint directly (with `v`-prefix tolerance)
 *   instead of relying on the broader `/releases?per_page=30` probe. The
 *   probe is for "what is the latest eligible version" — not "is this
 *   specific pinned tag valid".
 */

import { execFileSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import {
  copyFileSync,
  createReadStream,
  createWriteStream,
  existsSync,
  lstatSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  readlinkSync,
  realpathSync,
  renameSync,
  rmSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { error, log, warn } from "./logger.js";
import {
  aftCacheBase,
  readInstalledMetaIn,
  readVersionCheck,
  shouldRecheckVersion,
  withInstallLock,
  writeVersionCheck,
} from "./lsp-cache.js";
import {
  assertSafeVersion,
  isSafeVersion,
  probeGithubReleases,
  stripTagV,
} from "./lsp-github-probe.js";
import {
  type Arch,
  detectHostPlatform,
  findGithubServerById,
  GITHUB_LSP_TABLE,
  type GithubServerSpec,
  type Platform,
} from "./lsp-github-table.js";
import { hasRootMarker, relevantExtensionsInProject } from "./lsp-project-relevance.js";

/* ─────────────────────────── cache layout ─────────────────────────── */

function ghCacheRoot(): string {
  return join(aftCacheBase(), "lsp-binaries");
}

function ghPackageDir(spec: GithubServerSpec): string {
  return join(ghCacheRoot(), spec.id);
}

function ghBinDir(spec: GithubServerSpec): string {
  return join(ghPackageDir(spec), "bin");
}

function ghExtractDir(spec: GithubServerSpec): string {
  return join(ghPackageDir(spec), "extracted");
}

const INSTALLED_META_FILE = ".aft-installed";

interface GithubInstalledMeta {
  version: string;
  installedAt: string;
  sha256?: string;
  binarySha256?: string;
  archiveSha256?: string;
}

/** Final binary path under our cache. */
export function ghBinaryPath(spec: GithubServerSpec, platform: Platform): string {
  const ext = platform === "win32" ? ".exe" : "";
  return join(ghBinDir(spec), `${spec.binary}${ext}`);
}

export function isGithubInstalled(spec: GithubServerSpec, platform: Platform): boolean {
  for (const candidate of ghBinaryCandidates(spec, platform)) {
    try {
      if (statSync(join(ghBinDir(spec), candidate)).isFile()) return true;
    } catch {
      // Try the next Windows shim extension.
    }
  }
  return false;
}

function ghBinaryCandidates(spec: GithubServerSpec, platform: Platform): string[] {
  if (platform !== "win32") return [spec.binary];
  return [spec.binary, `${spec.binary}.cmd`, `${spec.binary}.exe`, `${spec.binary}.bat`];
}

function readGithubInstalledMetaIn(installDir: string): GithubInstalledMeta | null {
  try {
    const path = join(installDir, INSTALLED_META_FILE);
    if (!statSync(path).isFile()) return null;
    const parsed = JSON.parse(readFileSync(path, "utf8")) as Partial<GithubInstalledMeta>;
    if (typeof parsed.version !== "string" || parsed.version.length === 0) return null;
    return {
      version: parsed.version,
      installedAt: typeof parsed.installedAt === "string" ? parsed.installedAt : "",
      ...(typeof parsed.sha256 === "string" && parsed.sha256.length > 0
        ? { sha256: parsed.sha256 }
        : {}),
      ...(typeof parsed.binarySha256 === "string" && parsed.binarySha256.length > 0
        ? { binarySha256: parsed.binarySha256 }
        : {}),
      ...(typeof parsed.archiveSha256 === "string" && parsed.archiveSha256.length > 0
        ? { archiveSha256: parsed.archiveSha256 }
        : {}),
    };
  } catch {
    return null;
  }
}

function writeGithubInstalledMetaIn(
  installDir: string,
  version: string,
  binarySha256: string,
  archiveSha256?: string,
): void {
  try {
    mkdirSync(installDir, { recursive: true });
    const meta: GithubInstalledMeta = {
      version,
      installedAt: new Date().toISOString(),
      sha256: binarySha256,
      binarySha256,
      ...(archiveSha256 ? { archiveSha256 } : {}),
    };
    writeFileSync(join(installDir, INSTALLED_META_FILE), JSON.stringify(meta), "utf8");
  } catch (err) {
    warn(`[lsp] failed to write github installed metadata in ${installDir}: ${err}`);
  }
}

/* ─────────────────────────── per-call config ─────────────────────────── */

export interface GithubInstallConfig {
  autoInstall: boolean;
  graceDays: number;
  /** Per-package version pin map; key is `owner/repo`. Bypasses grace. */
  versions: Readonly<Record<string, string>>;
  disabled: ReadonlySet<string>;
}

/* ─────────────────────────── safety constants ─────────────────────────── */

/**
 * Hard cap on a single LSP-binary download (audit #4: GitHub downloads
 * unbounded).
 *
 * 256 MB. The largest currently-shipped LSP we install is `clangd` at
 * around 60 MB extracted (compressed less). The next-largest is
 * `lua-language-server` at ~20 MB. Pinning at 256 MB leaves a 4× headroom
 * for future-proofing while still aborting absurd payloads (TB-sized
 * malicious responses, DoS via slow drip).
 *
 * If a legitimate release ever exceeds this, we'll bump it deliberately
 * and the version-pin escape hatch lets users opt back in via `lsp.versions`.
 */
const MAX_DOWNLOAD_BYTES = 256 * 1024 * 1024;

/**
 * Maximum total uncompressed size for the extracted contents of one LSP
 * archive (1 GiB). Audit v0.17 #2: even with `MAX_DOWNLOAD_BYTES` capping
 * the compressed payload, modern compressors can achieve very high ratios
 * on uniform data — a 256 MB ZIP can decompress to dozens of GBs of zeros
 * and fill the user's disk before any individual file looks suspicious.
 */
const MAX_EXTRACT_BYTES = 1024 * 1024 * 1024;

/**
 * Compute the SHA-256 of `path` by streaming.
 * Audit v0.17 #1: enables hash logging + TOFU verification.
 */
function sha256OfFile(path: string): Promise<string> {
  return new Promise((resolve, reject) => {
    const hash = createHash("sha256");
    const stream = createReadStream(path);
    stream.on("error", reject);
    stream.on("data", (chunk) => hash.update(chunk));
    stream.on("end", () => resolve(hash.digest("hex")));
  });
}

function sha256OfFileSync(path: string): string {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

/* ─────────────────────────── pin resolution (audit #5) ─────────────────────────── */

/**
 * Fetch a single release by tag from GitHub's `/releases/tags/<tag>` endpoint.
 *
 * Critical for `lsp.versions` pin support: the broader release-list probe
 * only sees the most recent ~30 releases, so any older pin (clangd 18, an
 * old terraform, etc.) returns no assets and the install path silently
 * fails. The release-by-tag endpoint accepts arbitrary tag strings and
 * returns full asset metadata.
 *
 * We try the user's pin verbatim first, then with a `v` prefix added if
 * missing. GitHub release tags are inconsistent (clangd uses `21.1.0`,
 * lua-language-server uses `3.15.0`, zls uses `0.16.0`, terraform-ls uses
 * `v0.36.5`). User pins should not need to know each project's convention.
 *
 * Returns null on probe failure so the caller can fall back to keeping
 * any existing install.
 */
async function fetchReleaseByTag(
  githubRepo: string,
  tag: string,
  fetchImpl: typeof fetch,
  signal?: AbortSignal,
): Promise<{ tag: string; assets: Array<{ name: string; url: string; size?: number }> } | null> {
  const candidates: string[] = [];
  candidates.push(tag);
  if (!tag.startsWith("v")) {
    candidates.push(`v${tag}`);
  } else {
    candidates.push(tag.slice(1));
  }

  const headers: Record<string, string> = {
    accept: "application/vnd.github+json",
    "user-agent": "aft-opencode",
    "x-github-api-version": "2022-11-28",
  };
  if (process.env.GITHUB_TOKEN) {
    headers.authorization = `Bearer ${process.env.GITHUB_TOKEN}`;
  }

  for (const candidate of candidates) {
    const url = `https://api.github.com/repos/${githubRepo}/releases/tags/${encodeURIComponent(candidate)}`;
    const timeout = controlledTimeoutSignal(15_000, signal);
    try {
      const res = await fetchJsonFollowingRedirects(url, headers, fetchImpl, timeout.signal);
      if (res.status === 404) continue; // try next candidate
      if (!res.ok) {
        warn(`[lsp] github release-by-tag ${githubRepo}@${candidate}: HTTP ${res.status}`);
        return null;
      }
      const json = (await res.json()) as {
        tag_name?: string;
        assets?: Array<{ name?: string; browser_download_url?: string; size?: number }>;
      };
      if (!json.tag_name || !Array.isArray(json.assets)) {
        warn(`[lsp] github release-by-tag ${githubRepo}@${candidate}: malformed response`);
        return null;
      }
      const assets = json.assets
        .filter((a) => typeof a.name === "string" && typeof a.browser_download_url === "string")
        .map((a) => ({
          name: a.name as string,
          url: a.browser_download_url as string,
          size: typeof a.size === "number" ? a.size : undefined,
        }));
      return { tag: json.tag_name, assets };
    } catch (err) {
      if (signal?.aborted) {
        warn(`[lsp] github release-by-tag ${githubRepo}@${candidate}: aborted`);
        return null;
      }
      warn(`[lsp] github release-by-tag ${githubRepo}@${candidate}: ${err}`);
      // try next candidate
    } finally {
      timeout.cleanup();
    }
  }
  return null;
}

async function fetchJsonFollowingRedirects(
  url: string,
  headers: Record<string, string>,
  fetchImpl: typeof fetch,
  signal?: AbortSignal,
): Promise<Response> {
  const maxRedirects = 5;
  let currentUrl = url;
  for (let i = 0; i <= maxRedirects; i += 1) {
    assertAllowedDownloadUrl(currentUrl);
    const res = await fetchImpl(currentUrl, { headers, redirect: "manual", signal });
    if (res.status >= 300 && res.status < 400) {
      const location = res.headers.get("location");
      if (!location) throw new Error(`redirect status ${res.status} without Location`);
      currentUrl = new URL(location, currentUrl).toString();
      continue;
    }
    return res;
  }
  throw new Error(`too many redirects (>${maxRedirects})`);
}

/* ─────────────────────────── orchestrator ─────────────────────────── */

/**
 * Resolve which GitHub release tag to install, honoring user pins,
 * cached version checks, and the 7-day grace window.
 */
async function resolveTargetTag(
  spec: GithubServerSpec,
  config: GithubInstallConfig,
  fetchImpl: typeof fetch,
  signal?: AbortSignal,
): Promise<{
  tag: string | null;
  assets: Array<{ name: string; url: string; size?: number }>;
  blockedByGrace: boolean;
  reason?: string;
}> {
  // 1. User pin via `lsp.versions: { "clangd/clangd": "21.1.0" }`.
  //
  // Audit #5 fix: use GitHub's release-by-tag endpoint directly. The
  // previous code tried to find the pin in the latest-releases probe
  // and returned `assets: []` if it wasn't there, silently breaking the
  // documented escape hatch for any pin older than the latest 30 releases.
  const pinned = config.versions[spec.githubRepo];
  if (pinned) {
    // Audit v0.17 #3 + #13: validate the pin defense-in-depth before it
    // flows into URL paths and (eventually, via the probed tag) filesystem
    // paths and `tar -xf`.
    try {
      assertSafeVersion(pinned);
    } catch (err) {
      return {
        tag: null,
        assets: [],
        blockedByGrace: false,
        reason: `invalid pinned version ${JSON.stringify(pinned)}: ${err instanceof Error ? err.message : String(err)}`,
      };
    }
    const release = await fetchReleaseByTag(spec.githubRepo, pinned, fetchImpl, signal);
    if (release) {
      return {
        tag: release.tag,
        assets: release.assets,
        blockedByGrace: false,
      };
    }
    return {
      tag: null,
      assets: [],
      blockedByGrace: false,
      reason: `pinned tag ${pinned} not found on GitHub`,
    };
  }

  // 2. Cached check still fresh.
  //
  // Audit-2 v0.17 #3: previously this branch always probed the network
  // even with fresh cache, then on failure fell through to a second
  // probe and returned `{ tag: cached, assets: [] }` — a non-null tag
  // with empty assets that produced misleading "asset not found" errors.
  //
  // Fix: when cache is fresh, fetch assets for the cached tag directly.
  // If that lookup fails, fall through to live probe. If live probe also
  // fails, return tag:null so the caller skips cleanly.
  // Audit-3 v0.17 #2: validate cached.latest_eligible before consuming.
  const cached = readVersionCheck(spec.githubRepo);
  const weeklyMs = config.graceDays * 24 * 60 * 60 * 1000;
  const cachedTag = cached?.latest_eligible ?? null;
  const cachedSafe = isSafeVersion(cachedTag);
  if (cached && !shouldRecheckVersion(cached, weeklyMs) && cachedSafe) {
    const release = await fetchReleaseByTag(spec.githubRepo, cachedTag as string, fetchImpl);
    if (release) {
      return {
        tag: release.tag,
        assets: release.assets,
        blockedByGrace: false,
      };
    }
  }

  // 3. Live probe.
  const probe = await probeGithubReleases(spec.githubRepo, config.graceDays, fetchImpl);
  if (!probe) {
    // Audit-2 v0.17 #3: return tag:null on probe failure so the caller
    // skips cleanly instead of trying to install with empty assets.
    return {
      tag: null,
      assets: [],
      blockedByGrace: false,
      reason: "github releases probe failed",
    };
  }
  writeVersionCheck(spec.githubRepo, probe.tag);
  return { tag: probe.tag, assets: probe.assets, blockedByGrace: probe.blockedByGrace };
}

/* ─────────────────────────── download (audit #4) ─────────────────────────── */

function controlledTimeoutSignal(
  timeoutMs: number,
  parent?: AbortSignal,
): { signal: AbortSignal; cleanup: () => void } {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);
  timeout.unref?.();

  const abort = () => controller.abort();
  parent?.addEventListener("abort", abort, { once: true });
  if (parent?.aborted) abort();

  return {
    signal: controller.signal,
    cleanup: () => {
      clearTimeout(timeout);
      parent?.removeEventListener("abort", abort);
    },
  };
}

/**
 * Stream a remote URL to disk with a hard byte cap.
 *
 * Audit #4 fix: previously the download was unbounded, so a malicious or
 * misconfigured release could fill the user's disk and stall plugin
 * startup forever. We now:
 *   - Reject if `Content-Length` (when present) exceeds the cap.
 *   - Track streamed bytes during the pipeline and abort if the cap is
 *     exceeded mid-download. This handles the case where the server lies
 *     about Content-Length, omits it (chunked transfer), or sends more
 *     bytes than advertised.
 *
 * `assetSize` is the size hint from GitHub's release JSON. When provided
 * we sanity-check it against the cap before even starting the download.
 */
/**
 * Audit-3 v0.17 #5: hostname allowlist. browser_download_url from the
 * GitHub API is attacker-controllable; reject anything that is not on
 * a github.com / githubusercontent.com host before any network I/O.
 */
const ALLOWED_DOWNLOAD_HOSTS = new Set([
  "github.com",
  "api.github.com",
  "objects.githubusercontent.com",
  "release-assets.githubusercontent.com",
  "raw.githubusercontent.com",
  "codeload.github.com",
]);

function assertAllowedDownloadUrl(rawUrl: string): URL {
  let parsed: URL;
  try {
    parsed = new URL(rawUrl);
  } catch {
    throw new Error(`download url is not a valid URL: ${rawUrl}`);
  }
  if (parsed.protocol !== "https:") {
    throw new Error(`download url must be https (got ${parsed.protocol}): ${rawUrl}`);
  }
  if (!ALLOWED_DOWNLOAD_HOSTS.has(parsed.hostname.toLowerCase())) {
    throw new Error(
      `download url host ${parsed.hostname} is not in the GitHub allowlist: ${rawUrl}`,
    );
  }
  return parsed;
}

async function downloadFile(
  url: string,
  destPath: string,
  fetchImpl: typeof fetch,
  assetSize?: number,
  signal?: AbortSignal,
): Promise<void> {
  // Audit-3 v0.17 #5: enforce hostname allowlist before any network I/O.
  if (assetSize !== undefined && assetSize > MAX_DOWNLOAD_BYTES) {
    throw new Error(
      `asset size ${assetSize} exceeds max ${MAX_DOWNLOAD_BYTES} (set lsp.versions to pin a smaller release if this is wrong)`,
    );
  }

  const timeout = controlledTimeoutSignal(120_000, signal);
  try {
    const res = await fetchFollowingRedirects(url, fetchImpl, timeout.signal);
    if (!res.ok || !res.body) {
      throw new Error(`download failed (${res.status})`);
    }

    const advertised = Number.parseInt(res.headers.get("content-length") ?? "", 10);
    if (Number.isFinite(advertised) && advertised > MAX_DOWNLOAD_BYTES) {
      throw new Error(`Content-Length ${advertised} exceeds max ${MAX_DOWNLOAD_BYTES}`);
    }

    mkdirSync(dirname(destPath), { recursive: true });

    // Streaming size guard. Wrap the response body in a transformer that
    // counts bytes and aborts on overflow. If we don't do this we'll happily
    // stream a 100 GB malicious response straight to disk.
    let bytesWritten = 0;
    const guard = new TransformStream<Uint8Array, Uint8Array>({
      transform(chunk, controller) {
        bytesWritten += chunk.byteLength;
        if (bytesWritten > MAX_DOWNLOAD_BYTES) {
          controller.error(
            new Error(
              `download exceeded ${MAX_DOWNLOAD_BYTES} bytes after streaming (server lied about size or sent unbounded body)`,
            ),
          );
          return;
        }
        controller.enqueue(chunk);
      },
    });

    const guarded = res.body.pipeThrough(guard);
    // biome-ignore lint/suspicious/noExplicitAny: ReadableStream→Node stream conversion
    const nodeStream = Readable.fromWeb(guarded as any);
    await pipeline(nodeStream, createWriteStream(destPath), { signal: timeout.signal });
  } catch (err) {
    // Always clean up the partial file on any download failure.
    try {
      unlinkSync(destPath);
    } catch {
      // ignore
    }
    throw err;
  } finally {
    timeout.cleanup();
  }
}

async function fetchFollowingRedirects(
  url: string,
  fetchImpl: typeof fetch,
  signal?: AbortSignal,
): Promise<Response> {
  const maxRedirects = 5;
  let currentUrl = url;
  for (let i = 0; i <= maxRedirects; i += 1) {
    assertAllowedDownloadUrl(currentUrl);
    const res = await fetchImpl(currentUrl, {
      headers: { accept: "application/octet-stream" },
      redirect: "manual",
      signal,
    });
    if (res.status >= 300 && res.status < 400) {
      const location = res.headers.get("location");
      if (!location) throw new Error(`redirect status ${res.status} without Location`);
      currentUrl = new URL(location, currentUrl).toString();
      continue;
    }
    return res;
  }
  throw new Error(`too many redirects (>${maxRedirects})`);
}

/* ─────────────────────────── extraction (audit #3) ─────────────────────────── */

/**
 * Recursively validate that every entry under `stagingRoot` is contained
 * within it (audit #3: zip-slip + symlink containment).
 *
 * Rejects:
 *   - Any symlink (regardless of where it points). Symlinks in LSP
 *     installs are extremely rare and would be a red flag.
 *   - Any hardlink that has multiple inodes pointing at the same file
 *     OUTSIDE the staging root (we can't easily detect this for hardlinks
 *     to other paths inside the same archive, but those aren't a containment
 *     escape).
 *   - Any entry whose `realpath()` resolves outside `stagingRoot`. This
 *     catches what the platform extractor missed if it failed to defend
 *     against `..` traversal.
 *
 * Throws on any violation. The caller cleans up the staging dir.
 */
export function validateExtraction(stagingRoot: string): void {
  const realStagingRoot = realpathSync(stagingRoot);
  let totalBytes = 0;

  const walk = (dir: string): void => {
    let entries: string[];
    try {
      entries = readdirSync(dir);
    } catch (err) {
      throw new Error(`failed to read staging dir ${dir}: ${err}`);
    }

    for (const entry of entries) {
      const full = join(dir, entry);

      // lstatSync does NOT follow symlinks, so we can detect them.
      let lst: ReturnType<typeof lstatSync>;
      try {
        lst = lstatSync(full);
      } catch (err) {
        throw new Error(`failed to lstat ${full}: ${err}`);
      }

      if (lst.isSymbolicLink()) {
        let target = "<unreadable>";
        try {
          target = readlinkSync(full);
        } catch {
          // ignore — we're rejecting either way
        }
        throw new Error(
          `archive contains symlink ${relative(realStagingRoot, full)} → ${target}; rejecting (zip-slip defense)`,
        );
      }

      // Verify the entry's REAL path stays under the staging root. This
      // is the canonical zip-slip check — if the platform extractor wrote
      // a file with `..` components in its path, realpath will resolve it
      // to wherever it actually landed.
      let realFull: string;
      try {
        realFull = realpathSync(full);
      } catch (err) {
        throw new Error(`failed to realpath ${full}: ${err}`);
      }

      const rel = relative(realStagingRoot, realFull);
      if (rel.startsWith("..") || resolve(realStagingRoot, rel) !== realFull) {
        throw new Error(
          `archive entry escapes staging root: ${full} → ${realFull} (zip-slip defense)`,
        );
      }

      if (lst.isDirectory()) {
        walk(full);
      } else if (lst.isFile()) {
        // Audit v0.17 #2: accumulate uncompressed sizes and abort early if
        // we cross the cap. Walking the whole tree first would let a
        // decompression bomb fill the disk before we noticed.
        totalBytes += lst.size;
        if (totalBytes > MAX_EXTRACT_BYTES) {
          throw new Error(
            `extracted archive exceeds ${MAX_EXTRACT_BYTES} bytes (decompression bomb defense): saw ${totalBytes} bytes before hitting the cap`,
          );
        }
      } else {
        // Sockets, FIFOs, character devices — none of these belong in an
        // LSP release. Reject defensively.
        throw new Error(`archive contains non-file/non-dir entry: ${full}`);
      }
    }
  };

  walk(realStagingRoot);
}

/**
 * Extract `archivePath` into `destDir` safely:
 *
 *   1. Stage extraction in `<destDir>.staging-<rand>/`.
 *   2. Run the platform extractor against the staging dir.
 *   3. Validate the staging tree (no symlinks, no escapes).
 *   4. Atomic rename: `staging → destDir`. Any prior `destDir` is removed first.
 *   5. Always cleanup staging on any failure.
 */
export function precheckArchiveContents(archivePath: string, archiveType: string): void {
  let totalBytes = 0;
  if (archiveType === "zip") {
    const out = execFileSync("unzip", ["-l", archivePath], { encoding: "utf8" });
    const match = out.match(/^\s*(\d+)\s+\d+\s+files?\s*$/m);
    if (match) totalBytes = Number.parseInt(match[1] ?? "0", 10);
  } else {
    const out = execFileSync("tar", ["-tvf", archivePath], { encoding: "utf8" });
    for (const line of out.split("\n")) {
      if (line.startsWith("h")) {
        throw new Error(`archive contains hardlink entry: ${line.trim()}`);
      }
      const parts = line.trim().split(/\s+/);
      if (parts.length >= 6) {
        const numeric = parts
          .map((part) => Number.parseInt(part, 10))
          .filter((value) => Number.isFinite(value) && value >= 0);
        if (numeric.length > 0) totalBytes += Math.max(...numeric);
      }
    }
  }
  if (totalBytes > MAX_EXTRACT_BYTES) {
    throw new Error(`archive uncompressed size ${totalBytes} exceeds ${MAX_EXTRACT_BYTES}`);
  }
}

function extractArchiveSafely(archivePath: string, destDir: string, archiveType: string): void {
  // Per-process random suffix avoids collisions if two installs of the
  // same package somehow race past the install lock (defense in depth).
  const suffix = randomBytes(8).toString("hex");
  const stagingDir = `${destDir}.staging-${suffix}`;

  // Cleanup any stale staging dir from a prior crashed install.
  try {
    rmSync(stagingDir, { recursive: true, force: true });
  } catch {
    // ignore
  }

  mkdirSync(stagingDir, { recursive: true });

  try {
    precheckArchiveContents(archivePath, archiveType);
    runPlatformExtractor(archivePath, stagingDir, archiveType);
    validateExtraction(stagingDir);

    // Atomic publish: remove old destDir THEN rename staging into place.
    // The order matters because rename-over-existing-dir fails on Windows.
    try {
      rmSync(destDir, { recursive: true, force: true });
    } catch {
      // ignore — final renameSync will fail loudly if removal didn't work
    }
    renameSync(stagingDir, destDir);
  } catch (err) {
    // Always clean up the staging dir on any failure path.
    try {
      rmSync(stagingDir, { recursive: true, force: true });
    } catch {
      // ignore — cleanup failures are not as bad as the original error
    }
    throw err;
  }
}

function quarantineCachedGithubInstall(spec: GithubServerSpec, reason: string): void {
  const packageDir = ghPackageDir(spec);
  const dest = join(ghCacheRoot(), ".quarantine", encodeURIComponent(spec.id), `${Date.now()}`);
  warn(`[lsp] tofu_mismatch ${spec.id}: ${reason}; quarantining ${packageDir} -> ${dest}`);
  try {
    mkdirSync(dirname(dest), { recursive: true });
    rmSync(dest, { recursive: true, force: true });
    renameSync(packageDir, dest);
  } catch (err) {
    warn(`[lsp] tofu_mismatch ${spec.id}: failed to quarantine cache entry: ${err}`);
  }
}

function validateCachedGithubInstall(spec: GithubServerSpec, platform: Platform): boolean {
  const packageDir = ghPackageDir(spec);
  const meta = readGithubInstalledMetaIn(packageDir);
  const binaryPath = ghBinaryCandidates(spec, platform)
    .map((candidate) => join(ghBinDir(spec), candidate))
    .find((candidate) => {
      try {
        return statSync(candidate).isFile();
      } catch {
        return false;
      }
    });
  if (!meta?.version || !isSafeVersion(meta.version) || !binaryPath) {
    quarantineCachedGithubInstall(spec, "missing/unsafe metadata or binary");
    return false;
  }
  const currentHash = sha256OfFileSync(binaryPath);
  const recordedBinaryHash = meta.binarySha256 ?? meta.sha256;
  if (recordedBinaryHash && currentHash === recordedBinaryHash) {
    if (meta.sha256 !== currentHash || meta.binarySha256 !== currentHash) {
      writeGithubInstalledMetaIn(packageDir, meta.version, currentHash, meta.archiveSha256);
    }
    return true;
  }

  if (meta.sha256 && !meta.binarySha256 && !meta.archiveSha256) {
    writeGithubInstalledMetaIn(packageDir, meta.version, currentHash, meta.sha256);
    return true;
  }

  if (!recordedBinaryHash) {
    quarantineCachedGithubInstall(spec, "missing binary sha256 metadata");
    return false;
  }

  if (currentHash !== recordedBinaryHash) {
    quarantineCachedGithubInstall(spec, `recorded ${recordedBinaryHash}, current ${currentHash}`);
    return false;
  }
  return true;
}

/**
 * Internal: run the platform extractor against a fresh empty target.
 *
 * The extractor's containment defenses are platform-dependent: modern
 * `unzip` and `tar` reject absolute paths and `..` components, but we
 * still validate post-extraction in case a defense slipped past.
 */
function runPlatformExtractor(archivePath: string, destDir: string, archiveType: string): void {
  if (archiveType === "zip") {
    if (process.platform === "win32") {
      // Audit-2 v0.17 #12: drop PowerShell. Even via execFileSync, PowerShell
      // applies its own quoting rules to `$args[N]` lookups that could allow
      // attacker-controlled fragments to escape. Windows 10 build 17063+ ships
      // tar.exe in System32 — execFileSync with argv has no shell parser in
      // the chain at all, which is unconditionally safer.
      execFileSync("tar.exe", ["-xf", archivePath, "-C", destDir], {
        stdio: "pipe",
        timeout: 180_000,
      });
      return;
    }
    execFileSync("unzip", ["-q", "-o", archivePath, "-d", destDir], {
      stdio: "pipe",
      timeout: 180_000,
    });
    return;
  }

  if (archiveType === "tar.gz") {
    execFileSync("tar", ["-xzf", archivePath, "-C", destDir], {
      stdio: "pipe",
      timeout: 180_000,
    });
    return;
  }

  if (archiveType === "tar.xz") {
    execFileSync("tar", ["-xf", archivePath, "-C", destDir], {
      stdio: "pipe",
      timeout: 180_000,
    });
    return;
  }

  throw new Error(`unsupported archive type: ${archiveType}`);
}

/* ─────────────────────────── install pipeline ─────────────────────────── */

/**
 * Run the download + extract + binary-place flow for a single server.
 * Returns archive and final-binary SHA-256 hashes on success, or null on any failure.
 *
 * Audit v0.17 #1: caller persists the hash in `.aft-installed` for TOFU
 * verification on subsequent installs of the same tag.
 */
async function downloadAndInstall(
  spec: GithubServerSpec,
  tag: string,
  assets: ReadonlyArray<{ name: string; url: string; size?: number }>,
  platform: Platform,
  arch: Arch,
  fetchImpl: typeof fetch,
  signal?: AbortSignal,
): Promise<{ archiveSha256: string; binarySha256: string } | null> {
  const version = stripTagV(tag);
  const expected = spec.resolveAsset(platform, arch, version);
  if (!expected) {
    warn(`[lsp] ${spec.id}: unsupported platform/arch combo ${platform}/${arch}`);
    return null;
  }

  const matchingAsset = assets.find((a) => a.name === expected.name);
  if (!matchingAsset) {
    warn(
      `[lsp] ${spec.id}: asset ${expected.name} not found in release ${tag} (${assets.length} assets available)`,
    );
    return null;
  }

  const pkgDir = ghPackageDir(spec);
  const extractDir = ghExtractDir(spec);
  const archivePath = join(pkgDir, expected.name);

  log(`[lsp] downloading ${spec.id} ${tag} → ${matchingAsset.url}`);
  try {
    await downloadFile(matchingAsset.url, archivePath, fetchImpl, matchingAsset.size, signal);
  } catch (err) {
    error(`[lsp] download ${spec.id} failed: ${err}`);
    return null;
  }

  // Audit v0.17 #1: SHA-256 always-log + TOFU verification.
  let archiveSha256: string;
  try {
    archiveSha256 = await sha256OfFile(archivePath);
  } catch (err) {
    error(`[lsp] hash ${spec.id} failed: ${err}`);
    try {
      unlinkSync(archivePath);
    } catch {}
    return null;
  }
  log(`[lsp] ${spec.id} ${tag} sha256=${archiveSha256}`);

  const previousMeta = readGithubInstalledMetaIn(ghPackageDir(spec));
  const previousArchiveSha256 =
    previousMeta?.archiveSha256 ?? (previousMeta?.binarySha256 ? undefined : previousMeta?.sha256);
  if (previousMeta && previousMeta.version === tag && previousArchiveSha256) {
    if (previousArchiveSha256 !== archiveSha256) {
      error(
        `[lsp] ${spec.id} ${tag}: TOFU sha256 mismatch — refusing install. ` +
          `Previously installed archive sha256=${previousArchiveSha256}, downloaded sha256=${archiveSha256}. ` +
          `This means the published release for tag ${tag} changed. Investigate before proceeding.`,
      );
      try {
        unlinkSync(archivePath);
      } catch {}
      return null;
    }
  }

  try {
    extractArchiveSafely(archivePath, extractDir, expected.archive);
  } catch (err) {
    error(`[lsp] extract ${spec.id} failed: ${err}`);
    return null;
  } finally {
    try {
      unlinkSync(archivePath);
    } catch {
      // ignore — leftover archive isn't critical
    }
  }

  const innerBinaryPath = join(extractDir, spec.binaryPathInArchive(platform, arch, version));
  if (!existsSync(innerBinaryPath)) {
    error(`[lsp] ${spec.id}: extracted binary not found at ${innerBinaryPath}`);
    return null;
  }

  const targetBinary = ghBinaryPath(spec, platform);
  mkdirSync(dirname(targetBinary), { recursive: true });
  try {
    copyFileSync(innerBinaryPath, targetBinary);
    if (platform !== "win32") {
      // chmod +x.
      const { chmodSync } = await import("node:fs");
      chmodSync(targetBinary, 0o755);
    }
  } catch (err) {
    error(`[lsp] ${spec.id}: failed to place binary at ${targetBinary}: ${err}`);
    return null;
  }

  log(`[lsp] installed ${spec.id} ${tag} at ${targetBinary}`);
  const binarySha256 = await sha256OfFile(targetBinary);
  log(`[lsp] ${spec.id} ${tag} binary_sha256=${binarySha256}`);
  return { archiveSha256, binarySha256 };
}

/* ─────────────────────────── per-server flow (audit #2) ─────────────────────────── */

async function ensureGithubInstalled(
  spec: GithubServerSpec,
  config: GithubInstallConfig,
  fetchImpl: typeof fetch,
  platform: Platform,
  arch: Arch,
  signal?: AbortSignal,
): Promise<{ started: boolean; reason?: string }> {
  // Audit #2: hold the install lock through the FULL download+extract+install
  // cycle, not just the start decision. Two parallel sessions racing into
  // the same package would otherwise both pass the "already installed" check,
  // both claim the lock, both release it before downloading, and corrupt
  // the cache by extracting concurrent archives over each other.
  const outcome = await withInstallLock(spec.githubRepo, async () => {
    const { tag, assets, blockedByGrace, reason } = await resolveTargetTag(
      spec,
      config,
      fetchImpl,
      signal,
    );

    if (!tag) {
      const installed = isGithubInstalled(spec, platform);
      if (installed) {
        warn(
          `[lsp] no eligible release of ${spec.githubRepo} (grace=${config.graceDays}d); keeping existing install`,
        );
        return { started: false, reason: "kept existing install" };
      }
      const fallbackReason =
        reason ??
        (blockedByGrace
          ? `all releases are within ${config.graceDays}-day grace window`
          : "github releases probe failed");
      warn(`[lsp] skipping ${spec.id}: ${fallbackReason}`);
      return { started: false, reason: fallbackReason };
    }

    // Audit v0.17 #4: skip-if-installed must compare the installed tag
    // against the resolved target so pin changes take effect.
    if (isGithubInstalled(spec, platform)) {
      const installedMeta = readInstalledMetaIn(ghPackageDir(spec));
      if (installedMeta && installedMeta.version === tag) {
        return { started: false, reason: "already installed" };
      }
      if (installedMeta) {
        log(`[lsp] reinstalling ${spec.id}: cached ${installedMeta.version} ≠ target ${tag}`);
      } else {
        log(`[lsp] reinstalling ${spec.id}@${tag}: no installed-version metadata recorded`);
      }
    }

    // Hold the lock through the entire download+extract. Errors are logged
    // but we still return `started: true` so the caller counts the attempt.
    const hashes = await downloadAndInstall(
      spec,
      tag,
      assets,
      platform,
      arch,
      fetchImpl,
      signal,
    ).catch((err) => {
      error(`[lsp] github install ${spec.id} crashed: ${err}`);
      return null;
    });
    if (!hashes) {
      return { started: true, reason: "install failed (see plugin log)" };
    }
    writeGithubInstalledMetaIn(ghPackageDir(spec), tag, hashes.binarySha256, hashes.archiveSha256);
    return { started: true };
  });

  if (outcome === null) {
    return { started: false, reason: "another install in progress" };
  }
  return outcome;
}

/* ─────────────────────────── public entrypoint ─────────────────────────── */

export interface GithubAutoInstallResult {
  cachedBinDirs: string[];
  installsStarted: number;
  /** Binary names whose installs are actively in flight at return time. */
  installingBinaries: string[];
  skipped: Array<{ id: string; reason: string }>;
  /**
   * Promise that resolves when every backgrounded GitHub install settles.
   * Each install holds its per-package install lock for its full duration.
   * Plugin startup ignores this; tests await it.
   */
  installsComplete: Promise<void>;
}

interface InFlightGithubInstall {
  controller: AbortController;
  promise: Promise<void>;
}

const inFlightGithubInstalls = new Set<InFlightGithubInstall>();

function trackInFlightGithubInstall(
  controller: AbortController,
  promise: Promise<void>,
): Promise<void> {
  const entry: InFlightGithubInstall = { controller, promise };
  inFlightGithubInstalls.add(entry);
  promise.then(
    () => inFlightGithubInstalls.delete(entry),
    () => inFlightGithubInstalls.delete(entry),
  );
  return promise;
}

export async function abortInFlightGithubInstalls(): Promise<void> {
  const installs = Array.from(inFlightGithubInstalls);
  for (const install of installs) {
    install.controller.abort();
  }
  await Promise.allSettled(installs.map((install) => install.promise));
}

/**
 * Run the GitHub-release auto-install pass for every Pattern C server.
 *
 * Sync return: per-server cached bin dirs + skipped reasons known at decision
 * time. Backgrounded installs settle into `installsComplete` so plugin
 * startup is not blocked on slow downloads.
 */
export function runGithubAutoInstall(
  relevantServers: ReadonlySet<string>,
  config: GithubInstallConfig,
  fetchImpl: typeof fetch = fetch,
): GithubAutoInstallResult {
  const cachedBinDirs: string[] = [];
  const skipped: Array<{ id: string; reason: string }> = [];
  const installPromises: Promise<void>[] = [];
  const installingBinaries: string[] = [];
  let installsStarted = 0;

  const host = detectHostPlatform();
  if (!host) {
    // Unsupported host — skip every Pattern C install but surface cached
    // bin dirs so users with manually-installed binaries still benefit.
    for (const spec of GITHUB_LSP_TABLE) {
      try {
        if (existsSync(ghBinDir(spec))) {
          cachedBinDirs.push(ghBinDir(spec));
        }
      } catch {
        // ignore
      }
    }
    return {
      cachedBinDirs,
      installsStarted: 0,
      installingBinaries: [],
      skipped,
      installsComplete: Promise.resolve(),
    };
  }

  for (const spec of GITHUB_LSP_TABLE) {
    if (
      isGithubInstalled(spec, host.platform) &&
      validateCachedGithubInstall(spec, host.platform)
    ) {
      cachedBinDirs.push(ghBinDir(spec));
    }

    if (config.disabled.has(spec.id)) {
      skipped.push({ id: spec.id, reason: "disabled by config" });
      continue;
    }

    if (!config.autoInstall) {
      skipped.push({ id: spec.id, reason: "auto_install: false" });
      continue;
    }

    if (!relevantServers.has(spec.id)) {
      skipped.push({ id: spec.id, reason: "not relevant to project" });
      continue;
    }

    installsStarted += 1;
    installingBinaries.push(spec.binary);
    const controller = new AbortController();
    const promise = ensureGithubInstalled(
      spec,
      config,
      fetchImpl,
      host.platform,
      host.arch,
      controller.signal,
    ).then(
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
        error(`[lsp] github install ${spec.id} promise rejected: ${reason}`);
      },
    );
    installPromises.push(trackInFlightGithubInstall(controller, promise));
  }

  return {
    cachedBinDirs,
    installingBinaries,
    get installsStarted() {
      return installsStarted;
    },
    skipped,
    installsComplete: Promise.all(installPromises).then(() => {}),
  };
}

/* ─────────────────────────── discovery helper ─────────────────────────── */

/**
 * Cheap project-relevance scan for GitHub-distributed servers. Root markers
 * win immediately; otherwise use the bounded shared extension walk for
 * monorepos with nested source files.
 */
export function discoverRelevantGithubServers(projectRoot: string): Set<string> {
  // Same extension lists from the Rust registry.
  const extToServerIds: Record<string, string[]> = {
    c: ["clangd"],
    "c++": ["clangd"],
    cc: ["clangd"],
    cpp: ["clangd"],
    cxx: ["clangd"],
    h: ["clangd"],
    "h++": ["clangd"],
    hpp: ["clangd"],
    hh: ["clangd"],
    hxx: ["clangd"],
    lua: ["lua-ls"],
    zig: ["zls"],
    zon: ["zls"],
    typ: ["tinymist"],
    typc: ["tinymist"],
    tex: ["texlab"],
    bib: ["texlab"],
  };
  const rootMarkers: Record<string, readonly string[]> = {
    clangd: ["compile_commands.json", "compile_flags.txt", ".clangd"],
    "lua-ls": [".luarc.json", ".luarc.jsonc", ".stylua.toml", "stylua.toml"],
    zls: ["build.zig"],
    tinymist: ["typst.toml"],
    texlab: [".latexmkrc", "latexmkrc", ".texlabroot", "texlabroot"],
  };

  const relevant = new Set<string>();
  for (const spec of GITHUB_LSP_TABLE) {
    if (hasRootMarker(projectRoot, rootMarkers[spec.id])) relevant.add(spec.id);
  }

  const extensions = relevantExtensionsInProject(projectRoot, extToServerIds);
  for (const ext of extensions) {
    for (const id of extToServerIds[ext] ?? []) {
      relevant.add(id);
    }
  }
  return relevant;
}

/* ─────────────────────────── re-exports ─────────────────────────── */

/** Audit-3 v0.17 #5: test-only re-export. Production code uses it inline. */
export {
  type Arch,
  assertAllowedDownloadUrl as _assertAllowedDownloadUrlForTesting,
  detectHostPlatform,
  downloadFile as _downloadFileForTesting,
  findGithubServerById,
  GITHUB_LSP_TABLE,
  type GithubServerSpec,
  type Platform,
  precheckArchiveContents as _precheckArchiveSizeForTesting,
};
