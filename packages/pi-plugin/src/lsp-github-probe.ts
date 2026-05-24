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

import { warn } from "./logger.js";

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
): Promise<GithubVersionPickResult | null> {
  const url = `https://api.github.com/repos/${githubRepo}/releases?per_page=30`;
  try {
    const headers: Record<string, string> = { accept: "application/vnd.github+json" };
    if (process.env.GITHUB_TOKEN) {
      headers.authorization = `Bearer ${process.env.GITHUB_TOKEN}`;
    }
    const res = await fetchImpl(url, {
      headers,
      signal: AbortSignal.timeout(10_000),
    });
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
 * Audit v0.17 #3 + #13: tag/version strings flow into both file paths
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
 */
export function assertSafeVersion(version: string): void {
  if (!SAFE_VERSION_RE.test(version)) {
    throw new Error(
      `unsafe version/tag string ${JSON.stringify(version)}: must match ${SAFE_VERSION_RE.source}`,
    );
  }
}

/** Audit-3 v0.17 #2: non-throwing predicate for cache-miss-on-corruption flow. */
export function isSafeVersion(version: string | null | undefined): version is string {
  return typeof version === "string" && version.length > 0 && SAFE_VERSION_RE.test(version);
}

/**
 * Strip a leading `v` from a release tag to get a clean version for asset
 * templates that don't include the `v` prefix.
 *
 * Asserts the tag is safe via {@link assertSafeVersion} so callers can
 * pass it into paths and command arguments without additional validation.
 */
export function stripTagV(tag: string): string {
  assertSafeVersion(tag);
  return tag.startsWith("v") ? tag.slice(1) : tag;
}
