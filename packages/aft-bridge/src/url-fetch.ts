import { createHash } from "node:crypto";
import { lookup } from "node:dns/promises";
import {
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { isIP } from "node:net";
import { join } from "node:path";
import { Agent, type Dispatcher, fetch as undiciFetch } from "undici";
import { log, warn } from "./active-logger.js";

/** Max response body size (10 MB) */
const MAX_RESPONSE_BYTES = 10 * 1024 * 1024;
/** Cache TTL: 1 day */
const CACHE_TTL_MS = 24 * 60 * 60 * 1000;
/** Fetch timeout: 30 seconds */
const FETCH_TIMEOUT_MS = 30_000;
const MAX_REDIRECTS = 5;

interface FetchUrlOptions {
  allowPrivate?: boolean;
  /** Test-only injection point. Production fetches use undici with DNS pinning. */
  fetchImpl?: FetchImpl;
  /** Test-only injection point. Production DNS resolution uses node:dns/promises.lookup. */
  lookup?: LookupFn;
  /** Test-only injection point for observing the DNS-pinned dispatcher. */
  dispatcherFactory?: (validatedIp: string) => Dispatcher;
}

type LookupFn = typeof lookup;
interface FetchInit {
  signal?: AbortSignal;
  redirect?: "manual";
  dispatcher?: Dispatcher;
  headers?: Record<string, string>;
}

type FetchImpl = (input: string, init: FetchInit) => Promise<Response>;

interface CacheMeta {
  url: string;
  contentType: string;
  extension: string;
  fetchedAt: number;
}

function cacheDir(storageDir: string): string {
  return join(storageDir, "url_cache");
}

function hashUrl(url: string): string {
  return createHash("sha256").update(url).digest("hex").slice(0, 16);
}

function metaPath(storageDir: string, hash: string): string {
  return join(cacheDir(storageDir), `${hash}.meta.json`);
}

function contentPath(storageDir: string, hash: string, extension: string): string {
  return join(cacheDir(storageDir), `${hash}${extension}`);
}

/** Exported for unit tests. */
export function _isPrivateIpv4(address: string): boolean {
  const parts = address.split(".").map((part) => Number.parseInt(part, 10));
  if (parts.length !== 4 || parts.some((part) => !Number.isInteger(part))) return true;
  const [a, b] = parts;
  return (
    a === 0 || // 0.0.0.0/8 — wildcard, blocked to prevent local-host bypass
    a === 10 ||
    a === 127 ||
    (a === 172 && b >= 16 && b <= 31) ||
    (a === 192 && b === 168) ||
    (a === 169 && b === 254) ||
    a >= 224 // multicast (224/4) and reserved (240/4)
  );
}

/**
 * Expand a possibly-compressed IPv6 address to 8 hextets (numbers).
 * Returns null on parse failure.
 */
function expandIpv6(addr: string): number[] | null {
  if (addr === "::") return [0, 0, 0, 0, 0, 0, 0, 0];
  // Reject more than one "::"
  const dcMatches = addr.match(/::/g);
  if (dcMatches && dcMatches.length > 1) return null;

  // Handle dotted-quad tail by converting to two hextets.
  let normalized = addr;
  const lastColon = normalized.lastIndexOf(":");
  if (lastColon !== -1) {
    const tail = normalized.slice(lastColon + 1);
    if (tail.includes(".")) {
      const octets = tail.split(".").map((p) => Number.parseInt(p, 10));
      if (octets.length !== 4 || octets.some((o) => Number.isNaN(o) || o < 0 || o > 255)) {
        return null;
      }
      const h1 = ((octets[0] << 8) | octets[1]).toString(16);
      const h2 = ((octets[2] << 8) | octets[3]).toString(16);
      normalized = `${normalized.slice(0, lastColon)}:${h1}:${h2}`;
    }
  }

  let parts: string[];
  if (normalized.includes("::")) {
    const [left, right] = normalized.split("::");
    const leftParts = left ? left.split(":") : [];
    const rightParts = right ? right.split(":") : [];
    const fill = 8 - leftParts.length - rightParts.length;
    if (fill < 0) return null;
    parts = [...leftParts, ...Array(fill).fill("0"), ...rightParts];
  } else {
    parts = normalized.split(":");
  }

  if (parts.length !== 8) return null;
  const hextets = parts.map((p) => {
    const n = Number.parseInt(p, 16);
    return Number.isNaN(n) || n < 0 || n > 0xffff ? -1 : n;
  });
  if (hextets.some((h) => h === -1)) return null;
  return hextets;
}

function isPrivateIp(address: string): boolean {
  if (!address.includes(":")) {
    return _isPrivateIpv4(address);
  }

  const lower = address.toLowerCase();
  // Strip optional zone identifier (fe80::1%eth0 → fe80::1).
  const noZone = lower.split("%")[0];

  // Expand to 8 hextets so we can reliably detect IPv4-mapped/compatible
  // forms even after URL canonicalization (e.g. [::ffff:127.0.0.1] →
  // [::ffff:7f00:1]).
  const hextets = expandIpv6(noZone);
  if (hextets) {
    // IPv4-mapped (::ffff:X.X.X.X) and IPv4-compatible (::X.X.X.X) — extract
    // the embedded IPv4 from the last two hextets and check against IPv4 ranges.
    const top6Zero = hextets.slice(0, 6).every((h) => h === 0);
    const isMapped =
      hextets[0] === 0 &&
      hextets[1] === 0 &&
      hextets[2] === 0 &&
      hextets[3] === 0 &&
      hextets[4] === 0 &&
      hextets[5] === 0xffff;
    if (isMapped || top6Zero) {
      const a = (hextets[6] >> 8) & 0xff;
      const b = hextets[6] & 0xff;
      const c = (hextets[7] >> 8) & 0xff;
      const d = hextets[7] & 0xff;
      const ipv4 = `${a}.${b}.${c}.${d}`;
      // ::1 (loopback) and :: (unspecified) both fall into top6Zero and last
      // hextet 0 or 1 — _isPrivateIpv4 already blocks 0.0.0.0 and 127/8.
      if (top6Zero && hextets[6] === 0 && hextets[7] <= 1) {
        return true;
      }
      return _isPrivateIpv4(ipv4);
    }

    const firstHextet = hextets[0];
    return (
      // fe80::/10 link-local
      (firstHextet >= 0xfe80 && firstHextet <= 0xfebf) ||
      // fc00::/7 unique local
      (firstHextet >= 0xfc00 && firstHextet <= 0xfdff) ||
      // ff00::/8 multicast
      firstHextet >= 0xff00
    );
  }

  // Could not expand — be conservative and block.
  return true;
}

async function assertPublicUrl(
  url: URL,
  allowPrivate: boolean,
  dnsLookup: LookupFn = lookup,
): Promise<string | undefined> {
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new Error(`Only http:// and https:// URLs are supported, got: ${url.protocol}`);
  }
  if (allowPrivate) return undefined;

  // URL.hostname returns IPv6 literals wrapped in brackets ("[::1]") per WHATWG.
  // Strip them before checking the address.
  const hostname = url.hostname.replace(/^\[|\]$/g, "");

  // If the hostname is already a literal IP, check it directly — skip DNS so we
  // also catch malformed IPv6 forms (::127.0.0.1, ::ffff:127.0.0.1) that some
  // resolvers reject as "not found" instead of returning the embedded IPv4.
  if (isIP(hostname) || hostname.includes(":")) {
    if (isPrivateIp(hostname)) {
      throw new Error(`Blocked private URL host ${url.hostname} (${hostname})`);
    }
    return hostname;
  }

  const addresses = await dnsLookup(hostname, { all: true, verbatim: true });
  for (const { address } of addresses) {
    if (isPrivateIp(address)) {
      throw new Error(`Blocked private URL host ${url.hostname} (${address})`);
    }
  }
  if (addresses.length === 0) {
    throw new Error(`Failed to resolve URL host ${url.hostname}`);
  }
  // Prefer IPv4 when available. AAAA can come first on many resolvers, but
  // many user networks (corporate, home, mobile) don't have IPv6 transit, so
  // pinning to AAAA causes EHOSTUNREACH / connect timeouts that surface as a
  // bare `fetch failed`. Falls back to whatever DNS gave us if no IPv4 record.
  // `family` is documented as numeric (4/6) but be defensive — some runtimes
  // have surfaced it as the string "IPv4"/"IPv6" historically.
  const ipv4 = addresses.find(
    (entry) =>
      entry.family === 4 ||
      (entry.family as unknown) === "IPv4" ||
      (typeof entry.address === "string" && isIP(entry.address) === 4),
  );
  const chosen = ipv4 ?? addresses[0];
  if (!chosen || typeof chosen.address !== "string" || chosen.address.length === 0) {
    // Should be unreachable given the length>0 check above, but the bare
    // `Invalid IP address: undefined` failure surfaced by undici when an
    // empty/undefined IP slips into connect.lookup is opaque enough that
    // we want a clear, structured failure instead.
    throw new Error(
      `DNS lookup for ${url.hostname} returned no usable address (got: ${JSON.stringify(addresses)})`,
    );
  }
  return chosen.address;
}

function createPinnedDispatcher(validatedIp: string): Dispatcher {
  const family = validatedIp.includes(":") ? 6 : 4;
  // Node 18+ calls the lookup with `opts.all: true` (especially via TLS /
  // happy-eyeballs paths). When `all` is true the callback expects an *array*
  // of `{address, family}` objects, not the legacy `(err, address, family)`
  // 3-arg shape. Calling the legacy form there causes Node to surface
  // `TypeError [ERR_INVALID_IP_ADDRESS]: Invalid IP address: undefined` from
  // net:emitLookup, wrapped by undici as a bare `fetch failed`. Honor
  // whichever shape `opts.all` requested.
  // (Cast to `any` because undici's `LookupFunction` type only models the
  // single-address callback overload, not the `all: true` variadic.)
  // biome-ignore lint/suspicious/noExplicitAny: dual-overload lookup callback
  const pinnedLookup: any = (
    _hostname: string,
    opts: { all?: boolean } | undefined,
    // biome-ignore lint/suspicious/noExplicitAny: dual-overload lookup callback
    callback: any,
  ) => {
    if (opts?.all) {
      callback(null, [{ address: validatedIp, family }]);
    } else {
      callback(null, validatedIp, family);
    }
  };
  return new Agent({
    connect: { lookup: pinnedLookup },
  });
}

function resolveRedirectUrl(currentUrl: URL, location: string | null): URL {
  if (!location) {
    throw new Error(`Redirect from ${currentUrl.href} missing Location header`);
  }
  return new URL(location, currentUrl);
}

async function fetchWithRedirects(
  startUrl: URL,
  allowPrivate: boolean,
  deps: Pick<FetchUrlOptions, "dispatcherFactory" | "fetchImpl" | "lookup"> = {},
): Promise<Response> {
  let currentUrl = startUrl;
  const fetchImpl = deps.fetchImpl ?? (undiciFetch as FetchImpl);
  const dnsLookup = deps.lookup ?? lookup;

  for (let redirectCount = 0; redirectCount <= MAX_REDIRECTS; redirectCount++) {
    const validatedIp = await assertPublicUrl(currentUrl, allowPrivate, dnsLookup);
    const dispatcher = validatedIp
      ? (deps.dispatcherFactory ?? createPinnedDispatcher)(validatedIp)
      : undefined;

    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), FETCH_TIMEOUT_MS);
    let response: Response;
    try {
      response = await fetchImpl(currentUrl.href, {
        signal: controller.signal,
        redirect: "manual",
        dispatcher,
        headers: {
          "user-agent": "aft-opencode-plugin",
          // Prioritize markdown for content-negotiating servers (GitHub API, many docs sites).
          // `application/vnd.github.raw` is GitHub's custom type — returns raw markdown from
          // repo file and readme endpoints. Falls back to HTML if markdown is not available.
          accept:
            "application/vnd.github.raw, text/markdown, text/x-markdown, text/html;q=0.9, text/plain;q=0.5",
        },
      });
    } catch (err) {
      // undici wraps the underlying network error (ECONNREFUSED / ETIMEDOUT /
      // EHOSTUNREACH / ENOTFOUND / cert errors) inside `.cause` and exposes a
      // bare "fetch failed" on the outer Error. Surfacing the cause's `code`
      // and message turns an unactionable failure into something the agent
      // (and the user) can actually triage.
      const error = err as Error & { cause?: unknown; code?: string };
      const cause = error.cause as
        | (Error & { code?: string; errno?: number; syscall?: string })
        | undefined;
      const causeCode = cause?.code ?? error.code;
      const causeMsg = cause?.message ?? "";
      const detail = causeCode
        ? causeMsg
          ? `${causeCode}: ${causeMsg}`
          : causeCode
        : (error.message ?? "unknown error");
      // Log the full chain so triage doesn't depend on the agent surface
      // forwarding the wrapped error message.
      warn(`URL fetch failed: ${currentUrl.href} — ${detail}`, {
        outerMessage: error.message,
        causeName: cause?.name,
        causeCode,
        causeErrno: cause?.errno,
        causeSyscall: cause?.syscall,
      });
      throw new Error(`Failed to fetch ${currentUrl.href}: ${detail}`);
    } finally {
      clearTimeout(timer);
    }

    if (response.status < 300 || response.status >= 400) {
      return response;
    }

    if (redirectCount === MAX_REDIRECTS) {
      throw new Error(`Too many redirects fetching ${startUrl.href}`);
    }
    currentUrl = resolveRedirectUrl(currentUrl, response.headers.get("location"));
  }

  throw new Error(`Too many redirects fetching ${startUrl.href}`);
}

/**
 * Map a Content-Type header to a file extension AFT can parse.
 * Returns null for unsupported types.
 */
function resolveExtension(contentType: string): string | null {
  // Normalize: strip parameters (after `;`) AND pick the first value if the server
  // echoed back a comma-separated list (GitHub API does this for /readme endpoints).
  const lower = contentType.toLowerCase().split(";")[0].split(",")[0].trim();
  if (
    lower === "text/html" ||
    lower === "application/xhtml+xml" ||
    lower === "application/vnd.github.html" ||
    lower === "application/vnd.github+html"
  ) {
    return ".html";
  }
  if (
    lower === "text/markdown" ||
    lower === "text/x-markdown" ||
    lower === "application/markdown" ||
    // GitHub API raw content type — returns raw markdown for README endpoints
    lower === "application/vnd.github.raw" ||
    lower === "application/vnd.github+raw" ||
    lower === "application/vnd.github.v3.raw"
  ) {
    return ".md";
  }
  if (lower === "text/plain") {
    // treat plain text as markdown so aft_outline can show headings if present
    return ".md";
  }
  return null;
}

/**
 * Fetch a URL to a cached temp file. Uses disk cache with 1-day TTL.
 * Returns the cached file path the Rust outline/zoom command can read.
 * Throws on errors (invalid URL, network failure, unsupported content type, oversized body).
 */
export async function fetchUrlToTempFile(
  url: string,
  storageDir: string,
  options: FetchUrlOptions = {},
): Promise<string> {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    throw new Error(`Invalid URL: ${url}`);
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new Error(`Only http:// and https:// URLs are supported, got: ${parsed.protocol}`);
  }
  const allowPrivate = options.allowPrivate === true;

  // Enforce the caller's current SSRF policy before consulting the disk cache.
  // Otherwise a URL previously cached with allowPrivate=true could be replayed
  // by a later allowPrivate=false call without re-validating the host.
  await assertPublicUrl(parsed, allowPrivate, options.lookup);

  const dir = cacheDir(storageDir);
  mkdirSync(dir, { recursive: true });

  const hash = hashUrl(url);
  const metaFile = metaPath(storageDir, hash);

  // Check cache
  if (existsSync(metaFile)) {
    try {
      const meta = JSON.parse(readFileSync(metaFile, "utf8")) as CacheMeta;
      const age = Date.now() - meta.fetchedAt;
      const cached = contentPath(storageDir, hash, meta.extension);
      if (age < CACHE_TTL_MS && existsSync(cached)) {
        log(`URL cache hit: ${url} (${Math.round(age / 1000)}s old)`);
        return cached;
      }
    } catch {
      // corrupted meta, re-fetch
    }
  }

  log(`Fetching URL: ${url}`);
  const response = await fetchWithRedirects(parsed, allowPrivate, options);

  if (!response.ok) {
    throw new Error(`HTTP ${response.status} ${response.statusText} fetching ${url}`);
  }

  const contentType = response.headers.get("content-type") || "text/plain";
  const extension = resolveExtension(contentType);
  if (!extension) {
    throw new Error(
      `Unsupported content type '${contentType}' for ${url}. Supported: text/html, text/markdown, text/plain`,
    );
  }

  const lengthHeader = response.headers.get("content-length");
  if (lengthHeader) {
    const length = Number.parseInt(lengthHeader, 10);
    if (Number.isFinite(length) && length > MAX_RESPONSE_BYTES) {
      throw new Error(`Response too large: ${length} bytes (max ${MAX_RESPONSE_BYTES})`);
    }
  }

  // Stream with size cap
  const reader = response.body?.getReader();
  if (!reader) {
    throw new Error(`Failed to read response body for ${url}`);
  }
  const chunks: Uint8Array[] = [];
  let total = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    if (value) {
      total += value.length;
      if (total > MAX_RESPONSE_BYTES) {
        reader.cancel().catch(() => {});
        throw new Error(`Response exceeded ${MAX_RESPONSE_BYTES} bytes, aborted`);
      }
      chunks.push(value);
    }
  }

  // Write content and meta atomically
  const body = Buffer.concat(chunks);
  const contentFile = contentPath(storageDir, hash, extension);
  const tmpContent = `${contentFile}.tmp-${process.pid}`;
  writeFileSync(tmpContent, body);
  const { renameSync } = await import("node:fs");
  renameSync(tmpContent, contentFile);

  const meta: CacheMeta = {
    url,
    contentType,
    extension,
    fetchedAt: Date.now(),
  };
  const tmpMeta = `${metaFile}.tmp-${process.pid}`;
  writeFileSync(tmpMeta, JSON.stringify(meta));
  renameSync(tmpMeta, metaFile);

  log(`URL cached (${total} bytes): ${url}`);
  return contentFile;
}

/**
 * Remove cache entries older than TTL. Called periodically at plugin startup.
 */
export function cleanupUrlCache(storageDir: string): void {
  const dir = cacheDir(storageDir);
  if (!existsSync(dir)) return;

  let removed = 0;
  try {
    for (const entry of readdirSync(dir)) {
      if (!entry.endsWith(".meta.json")) continue;
      const metaFile = join(dir, entry);
      try {
        const meta = JSON.parse(readFileSync(metaFile, "utf8")) as CacheMeta;
        const age = Date.now() - meta.fetchedAt;
        if (age > CACHE_TTL_MS) {
          const hash = entry.slice(0, -".meta.json".length);
          const content = contentPath(storageDir, hash, meta.extension);
          if (existsSync(content)) unlinkSync(content);
          unlinkSync(metaFile);
          removed++;
        }
      } catch {
        // corrupted meta, remove it too
        try {
          unlinkSync(metaFile);
          removed++;
        } catch {}
      }
    }
  } catch (err) {
    warn(`URL cache cleanup failed: ${(err as Error).message}`);
    return;
  }
  if (removed > 0) {
    log(`URL cache cleanup: removed ${removed} stale entries`);
  }
}
