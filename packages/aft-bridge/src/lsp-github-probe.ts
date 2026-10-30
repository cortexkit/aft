/**
 * Probes the GitHub releases API for a project's tagged releases and
 * selects the newest tag whose asset publication satisfies AFT's
 * supply-chain grace window.
 *
 * Same threat model as the npm registry probe — we want releases that
 * have been observable for at least `graceDays` so the community has
 * time to detect compromised tarballs and yank them.
 *
 * For repos that put release tags on a `published_at` field (almost
 * all of them), we filter on that. Pre-releases are skipped by default.
 */

import { warn as defaultWarn } from "./active-logger.js";

/**
 * Hosts we will fetch GitHub release metadata and artifacts from. The GitHub
 * API returns `browser_download_url` values that almost always live on
 * `github.com` (the canonical URL), but the actual blob is served from
 * `objects.githubusercontent.com` after redirect. We allow both.
 *
 * Without this allowlist, a compromised or malicious GitHub API response
 * could swap `browser_download_url` to an attacker-controlled host (e.g.
 * `evil.example/payload.zip`) and we would happily follow the redirect
 * chain. The download size cap and SHA-256 TOFU help but cannot stop the
 * very first install on a tag we have never seen.
 *
 * Redirects are handled manually so every hop is checked before any bytes
 * are accepted. A compromised release URL may start on github.com and
 * redirect to an attacker-controlled host; `redirect: "follow"` would hide
 * that hop.
 */
const ALLOWED_GITHUB_HOSTS = new Set([
  "github.com",
  "api.github.com",
  "objects.githubusercontent.com",
  "release-assets.githubusercontent.com",
  "raw.githubusercontent.com",
  "codeload.github.com",
]);

/** Throw unless `rawUrl` is an https URL on one of the allowed GitHub hosts. */
export function assertAllowedDownloadUrl(rawUrl: string): URL {
  let parsed: URL;
  try {
    parsed = new URL(rawUrl);
  } catch {
    throw new Error(`download url is not a valid URL: ${rawUrl}`);
  }
  if (parsed.protocol !== "https:") {
    throw new Error(`download url must be https (got ${parsed.protocol}): ${rawUrl}`);
  }
  if (!ALLOWED_GITHUB_HOSTS.has(parsed.hostname.toLowerCase())) {
    throw new Error(
      `download url host ${parsed.hostname} is not in the GitHub allowlist: ${rawUrl}`,
    );
  }
  return parsed;
}

/** The only origin that may receive the GitHub token. */
const GITHUB_API_ORIGIN = "https://api.github.com";
const MAX_GITHUB_REDIRECTS = 5;

function withoutAuthorization(headers: Record<string, string>): Record<string, string> {
  // Header names are case-insensitive, so `Authorization` must go too.
  return Object.fromEntries(
    Object.entries(headers).filter(([name]) => name.toLowerCase() !== "authorization"),
  );
}

/**
 * Fetch a GitHub API URL, following redirects by hand. Every hop must be on
 * an allowed GitHub host, and once a hop leaves the API origin the
 * Authorization header is dropped for that hop and every later one. The
 * runtime's own redirect handling is not relied on: whether it drops the
 * header on a cross-origin hop differs between runtimes.
 */
export async function fetchGithubApiFollowingRedirects(
  url: string,
  headers: Record<string, string>,
  fetchImpl: typeof fetch,
  signal?: AbortSignal,
): Promise<Response> {
  let currentUrl = url;
  let requestHeaders = { ...headers };
  for (let i = 0; i <= MAX_GITHUB_REDIRECTS; i += 1) {
    const parsed = assertAllowedDownloadUrl(currentUrl);
    if (parsed.origin !== GITHUB_API_ORIGIN) {
      requestHeaders = withoutAuthorization(requestHeaders);
    }
    const res = await fetchImpl(currentUrl, {
      headers: { ...requestHeaders },
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
  throw new Error(`too many redirects (>${MAX_GITHUB_REDIRECTS})`);
}

interface GithubRelease {
  tag_name: string;
  name?: string;
  published_at?: string;
  draft?: boolean;
  prerelease?: boolean;
  assets?: Array<{
    name: string;
    browser_download_url: string;
    size?: number;
  }>;
}

export interface GithubVersionPickResult {
  /** Chosen release tag (e.g. "v21.1.0" or "21.1.0"), or null if none qualifies. */
  tag: string | null;
  /** Asset list of the chosen release — used to find the right archive. */
  assets: Array<{ name: string; url: string; size?: number }>;
  /** True when releases exist but none is older than `graceDays`. */
  blockedByGrace: boolean;
}

/**
 * Pick the newest non-draft non-prerelease release whose `published_at`
 * is at least `graceDays` ago.
 *
 * The GitHub `/releases` endpoint returns up to 30 releases sorted newest
 * first by default — pagination ignored because servers we care about
 * cycle stable releases on the order of weeks/months.
 */
export function pickEligibleRelease(
  releases: readonly GithubRelease[],
  graceDays: number,
  now: number = Date.now(),
): GithubVersionPickResult {
  const cutoff = now - graceDays * 24 * 60 * 60 * 1000;

  const candidates = releases
    .filter((r) => !r.draft && !r.prerelease && typeof r.published_at === "string")
    .map((r) => {
      const ts = Date.parse(r.published_at as string);
      return { release: r, ts };
    })
    .filter((c) => !Number.isNaN(c.ts))
    .sort((a, b) => b.ts - a.ts);

  const eligible = candidates.filter((c) => c.ts <= cutoff);
  const blockedByGrace = candidates.length > 0 && eligible.length === 0;

  const chosen = eligible[0]?.release;
  if (!chosen) {
    return { tag: null, assets: [], blockedByGrace };
  }
  return {
    tag: chosen.tag_name,
    assets: (chosen.assets ?? []).map((a) => ({
      name: a.name,
      url: a.browser_download_url,
      size: a.size,
    })),
    blockedByGrace: false,
  };
}

/**
 * Fetch the GitHub releases list for `owner/repo` and apply the grace filter.
 *
 * Returns `null` on HTTP/network failure — caller should keep using whatever's
 * already cached.
 */
export async function probeGithubReleases(
  githubRepo: string,
  graceDays: number,
  fetchImpl: typeof fetch = fetch,
  warn: (message: string) => void = defaultWarn,
  signal?: AbortSignal,
): Promise<GithubVersionPickResult | null> {
  const url = `https://api.github.com/repos/${githubRepo}/releases?per_page=30`;
  try {
    const headers: Record<string, string> = { accept: "application/vnd.github+json" };
    if (process.env.GITHUB_TOKEN) {
      headers.authorization = `Bearer ${process.env.GITHUB_TOKEN}`;
    }
    const res = await fetchGithubApiFollowingRedirects(
      url,
      headers,
      fetchImpl,
      // The caller's signal (plugin shutdown) cancels the probe as well as
      // the fixed timeout.
      signal ? AbortSignal.any([signal, AbortSignal.timeout(10_000)]) : AbortSignal.timeout(10_000),
    );
    if (!res.ok) {
      warn(`[lsp] github releases probe failed for ${githubRepo}: HTTP ${res.status}`);
      return null;
    }
    const json = (await res.json()) as GithubRelease[];
    if (!Array.isArray(json)) {
      warn(`[lsp] unexpected response shape from github releases for ${githubRepo}`);
      return null;
    }
    return pickEligibleRelease(json, graceDays);
  } catch (err) {
    warn(`[lsp] github releases probe failed for ${githubRepo}: ${err}`);
    return null;
  }
}

/**
 * Allowed characters in a release tag/version string.
 *
 * Tag/version strings flow into both file paths
 * (`archivePath`) and shell-adjacent contexts. A hostile or compromised
 * release could publish tags like `1.0&calc.exe`, `1.0/../etc`, or
 * `v"; rm -rf /; #` to attempt path traversal or shell injection.
 *
 * The whitelist `[A-Za-z0-9._+-]+` covers all real-world version formats
 * (semver, calver, semver-with-pre/build, leading-`v`) without admitting
 * any character that has special meaning in cmd.exe, PowerShell, POSIX
 * shells, or filesystem paths.
 */
const SAFE_VERSION_RE = /^[A-Za-z0-9._+-]+$/;

/**
 * Throw if `version` contains anything outside the safe allowlist.
 *
 * Call this at every boundary where a version string is about to be
 * concatenated into a path, command, or URL — defense-in-depth even
 * when the surrounding code uses execFile (no shell).
 */
export function assertSafeVersion(version: string): void {
  if (!SAFE_VERSION_RE.test(version)) {
    throw new Error(
      `unsafe version/tag string ${JSON.stringify(version)}: must match ${SAFE_VERSION_RE.source}`,
    );
  }
}

/**
 * Non-throwing version of {@link assertSafeVersion}. Returns true when
 * `version` is a non-null, non-empty string that matches the allowlist.
 *
 * Callers that read version strings from disk caches
 * (where corruption is possible but not necessarily an attack) prefer
 * this over throwing — they degrade to "treat as cache miss" instead of
 * crashing the plugin.
 */
export function isSafeVersion(version: string | null | undefined): version is string {
  return typeof version === "string" && version.length > 0 && SAFE_VERSION_RE.test(version);
}

/**
 * Strip a leading `v` from a release tag to get a clean version for asset
 * templates that don't include the `v` prefix.
 *
 * The returned value is asserted safe via {@link assertSafeVersion} so
 * callers can pass it into paths and command arguments without
 * additional validation.
 */
export function stripTagV(tag: string): string {
  assertSafeVersion(tag);
  return tag.startsWith("v") ? tag.slice(1) : tag;
}
