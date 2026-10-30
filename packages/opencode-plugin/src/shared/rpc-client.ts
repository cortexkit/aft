import { existsSync, readdirSync, readFileSync, unlinkSync } from "node:fs";
import { join } from "node:path";
import { isPidAlive, parseRpcPortRecord, rpcPortFileDir, rpcPortFilePath } from "./rpc-utils";

const MAX_RETRIES = 10;
const RETRY_DELAY_MS = 500;
const REQUEST_TIMEOUT_MS = 5000;

type PortInfoSource = "instance" | "legacy";
type ParsedPortInfo = { port: number; token: string | null; pid?: number; started_at?: number };
type PortInfo = ParsedPortInfo & { source: PortInfoSource; path?: string };

export type AftRpcEndpoint = { port: number; token: string | null };

export interface AftRpcCallOptions {
  signal?: AbortSignal;
  /**
   * Optional gate over WARM (non-placeholder) responses. With multiple RPC
   * servers alive for one project hash (multi-project hosts, stale long-lived
   * processes), a warm response can describe ANOTHER project's bridge
   * (cross-project contamination). Return false to skip that response and
   * keep trying other ports instead of returning it — so a stray server can't
   * mask the right one. Skipped responses are never cached as the good port
   * and never returned; if only strays and placeholders answer, the
   * placeholder wins.
   */
  accept?: (result: unknown) => boolean;
  /**
   * Internal one-hop guard for verified-directory redirects. A redirected
   * call must never redirect again (placeholder chains would otherwise
   * ping-pong between two bridgeless instances).
   */
  noRedirect?: boolean;
}

/**
 * Verified-directory redirect registry (module-level, shared by every client
 * in this process).
 *
 * Why it exists: the TUI process can run in a DIFFERENT directory than the
 * process hosting the session's warm bridge (TUI attached to `opencode
 * serve` / Desktop, launched from $HOME). Port-file discovery is keyed by
 * hash(directory), so the TUI's client only ever finds its own bridgeless
 * instance — which answers the lazy placeholder forever. The placeholder now
 * carries `verified_directory` (the session's SDK-verified project root);
 * `call()` follows it once, to the port files of the instance that actually
 * owns the session's bridge.
 *
 * The registry remembers learned redirects so `resolveEndpoint()` (the
 * persistent notification WebSocket) can aim at the same instance, and
 * notifies listeners so an already-connected socket re-homes instead of
 * staying subscribed to the bridgeless instance that will never push.
 */
const redirectTargets = new Map<string, string>();
const redirectListeners = new Set<(from: string, to: string) => void>();

export function subscribeRpcRedirects(listener: (from: string, to: string) => void): () => void {
  redirectListeners.add(listener);
  return () => {
    redirectListeners.delete(listener);
  };
}

function recordRedirect(from: string, to: string): void {
  if (redirectTargets.get(from) === to) return;
  redirectTargets.set(from, to);
  for (const listener of redirectListeners) {
    try {
      listener(from, to);
    } catch {
      // one listener must not block the others
    }
  }
}

export function __resetRpcRedirectsForTest(): void {
  redirectTargets.clear();
  redirectListeners.clear();
}

export function __recordRpcRedirectForTest(from: string, to: string): void {
  recordRedirect(from, to);
}

function abortError(signal: AbortSignal): Error {
  const reason = signal.reason;
  if (reason instanceof Error) return reason;
  return new Error("AFT RPC request aborted");
}

function throwIfAborted(signal?: AbortSignal): void {
  if (signal?.aborted) throw abortError(signal);
}

function delay(ms: number, signal?: AbortSignal): Promise<void> {
  throwIfAborted(signal);
  return new Promise((resolve, reject) => {
    let timer: ReturnType<typeof setTimeout>;
    const onAbort = () => {
      clearTimeout(timer);
      reject(signal ? abortError(signal) : new Error("AFT RPC request aborted"));
    };
    timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

export class AftRpcClient {
  private port: number | null = null;
  private token: string | null = null;
  private portsDir: string;
  private legacyPortFile: string;
  private stalePortFailures = new Map<string, number>();
  private storageDir: string;
  private directory: string;

  constructor(storageDir: string, directory: string) {
    this.storageDir = storageDir;
    this.directory = directory;
    this.portsDir = rpcPortFileDir(storageDir, directory);
    this.legacyPortFile = rpcPortFilePath(storageDir, directory);
  }

  /** Call an RPC method. Retries port resolution if the server isn't ready yet. */
  async call<T = Record<string, unknown>>(
    method: string,
    params: Record<string, unknown> = {},
    options: AftRpcCallOptions = {},
  ): Promise<T> {
    const { signal } = options;
    throwIfAborted(signal);

    // Fast path: reuse the endpoint that served the last warm response while
    // it is still the newest live port file, skipping discovery's sequential
    // health checks. Anything other than an accepted warm response falls back
    // to the full scan below.
    let placeholder: T | null = null;
    let lastError: unknown = null;
    let answeredPort: number | null = null;
    const cached = this.cachedEndpoint();
    if (cached) {
      try {
        const result = await this.callOne<T>(method, params, cached, signal);
        if (this.looksLikePlaceholder(result)) {
          placeholder = result;
          answeredPort = cached.port;
        } else if (!options.accept || options.accept(result)) {
          return result;
        } else {
          answeredPort = cached.port;
        }
      } catch {
        throwIfAborted(signal);
      }
      this.port = null;
      this.token = null;
    }

    // Try ALL discovered ports for this project (OpenCode TUI under --port 0
    // loads our plugin twice in the same process, so two RPC servers listen
    // and we have to try both — only one's bridge is actually warm). A port
    // that already answered on the fast path is not asked again.
    const infos = (await this.resolvePortInfos(signal)).filter(
      (info) => info.port !== answeredPort,
    );
    if (infos.length === 0 && placeholder === null) {
      throw new Error("AFT RPC server not available");
    }

    // First pass: try every port. Prefer responses that look like "warm
    // bridge" output (i.e. not the synthetic `status: "not_initialized"`
    // placeholder served when this instance's bridge hasn't been spawned).
    for (const info of infos) {
      throwIfAborted(signal);
      try {
        const result = await this.callOne<T>(method, params, info, signal);
        if (this.looksLikePlaceholder(result)) {
          placeholder = result; // remember but keep trying
          continue;
        }
        if (options.accept && !options.accept(result)) {
          // Stray warm response (e.g. another project's data) — skip it and
          // keep scanning so it can't mask the right server.
          continue;
        }
        // Warm response — cache this port for subsequent calls.
        this.port = info.port;
        this.token = info.token;
        return result;
      } catch (err) {
        throwIfAborted(signal);
        lastError = err;
      }
    }

    // All ports returned placeholder OR failed. Before settling for the
    // placeholder, follow its verified_directory once: the session's warm
    // bridge may live in another process registered under a different
    // directory hash (TUI attached to a serve/Desktop host). See the
    // redirect registry doc above.
    if (placeholder !== null) {
      const redirected = await this.followVerifiedDirectory<T>(
        method,
        params,
        options,
        placeholder,
      );
      if (redirected !== null) return redirected;
      return placeholder;
    }
    throw lastError instanceof Error ? lastError : new Error(String(lastError));
  }

  /**
   * One-hop redirect to the instance that owns the session's bridge. Returns
   * null (caller keeps the placeholder) unless the redirected call produced a
   * warm response — a placeholder from the target is not an improvement, and
   * failures must not mask the original placeholder.
   */
  private async followVerifiedDirectory<T>(
    method: string,
    params: Record<string, unknown>,
    options: AftRpcCallOptions,
    placeholder: T,
  ): Promise<T | null> {
    if (options.noRedirect) return null;
    const verified = (placeholder as Record<string, unknown> | null)?.verified_directory;
    if (typeof verified !== "string" || verified.length === 0 || verified === this.directory) {
      return null;
    }
    const target = new AftRpcClient(this.storageDir, verified);
    try {
      const result = await target.call<T>(method, params, { ...options, noRedirect: true });
      if (this.looksLikePlaceholder(result)) return null;
      // Only remember (and re-home the notification socket) once the target
      // actually served warm data — a dead redirect must stay unlearned.
      recordRedirect(this.directory, verified);
      return result;
    } catch {
      return null;
    }
  }

  private async callOne<T>(
    method: string,
    params: Record<string, unknown>,
    info: PortInfo,
    signal?: AbortSignal,
  ): Promise<T> {
    return this.fetchWithTimeout(
      `http://127.0.0.1:${info.port}/rpc/${method}`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ ...params, token: info.token }),
      },
      signal,
      async (response) => {
        if (!response.ok) {
          const text = await response.text();
          throw new Error(`RPC ${method} failed (${response.status}): ${text}`);
        }
        return (await response.json()) as T;
      },
    );
  }

  /**
   * Heuristic for "this response is the lazy-spawn placeholder, not the real
   * data." We treat any `not_initialized` status as a placeholder so the
   * client knows to try the next port (the warm one).
   */
  private looksLikePlaceholder<T>(result: T): boolean {
    if (!result || typeof result !== "object") return false;
    const status = (result as Record<string, unknown>).status;
    return status === "not_initialized";
  }

  /**
   * The endpoint that served the last warm response, if it is still the newest
   * live port file for this project. Discovery tries ports newest first and
   * returns the first warm answer, so while this holds the full scan would
   * reach the same server first. Reading the port files also drops entries
   * whose owning process has died, so a dead server is never reused.
   */
  private cachedEndpoint(): PortInfo | null {
    if (this.port === null) return null;
    const [newest] = this.readAllPortFiles();
    if (!newest || newest.port !== this.port || newest.token !== this.token) return null;
    return newest;
  }

  /** Check if any RPC server is reachable. */
  async isAvailable(): Promise<boolean> {
    try {
      const infos = await this.resolvePortInfos();
      return infos.length > 0;
    } catch {
      return false;
    }
  }

  /** Resolve the freshest live endpoint for the persistent TUI WebSocket.
   *  Follows a learned verified-directory redirect so the socket subscribes
   *  to the instance that owns the session's bridge (the one that will
   *  actually push status-changed frames), not this directory's bridgeless
   *  placeholder instance. */
  async resolveEndpoint(signal?: AbortSignal): Promise<AftRpcEndpoint | null> {
    const redirect = redirectTargets.get(this.directory);
    if (redirect) {
      const target = new AftRpcClient(this.storageDir, redirect);
      const [info] = await target.resolvePortInfos(signal);
      if (info) return { port: info.port, token: info.token };
      // Redirect target gone (process exited) — fall through to own ports.
    }
    const [info] = await this.resolvePortInfos(signal);
    if (!info) return null;
    return { port: info.port, token: info.token };
  }

  /**
   * Discover all live RPC port files for this project. Tries the per-instance
   * directory first (v0.28.2+), then the single legacy `port` file (older
   * plugin versions in mixed deployments) as the final fallback candidate.
   */
  private async resolvePortInfos(signal?: AbortSignal): Promise<PortInfo[]> {
    for (let attempt = 0; attempt < MAX_RETRIES; attempt++) {
      throwIfAborted(signal);
      const infos = this.readAllPortFiles();
      if (infos.length > 0) {
        const alive: PortInfo[] = [];
        for (const info of infos) {
          throwIfAborted(signal);
          if (await this.healthCheck(info.port, info.pid, signal)) {
            this.clearPortFailure(info);
            alive.push(info);
          } else {
            this.recordPortFailure(info);
          }
        }
        if (alive.length > 0) return alive;
      }
      if (attempt < MAX_RETRIES - 1) {
        await delay(RETRY_DELAY_MS, signal);
      }
    }
    return [];
  }

  private readAllPortFiles(): PortInfo[] {
    const collected: PortInfo[] = [];
    const seenPorts = new Set<number>();
    const add = (info: PortInfo) => {
      if (seenPorts.has(info.port)) return;
      seenPorts.add(info.port);
      collected.push(info);
    };

    // Per-instance directory (v0.28.2+): one file per plugin load. Each file now
    // records the owning process pid; we skip (and reclaim) any whose process is
    // dead so a poll doesn't wade through crash/restart leftovers, and we order
    // newest-first so the freshest live server wins the port de-dupe (a stale
    // file naming a reused port can no longer mask a fresh one with its old token).
    if (existsSync(this.portsDir)) {
      try {
        const live: PortInfo[] = [];
        for (const entry of readdirSync(this.portsDir)) {
          if (!entry.endsWith(".json")) continue;
          const filePath = join(this.portsDir, entry);
          const info = this.parsePortFile(filePath);
          if (!info) continue;
          // Only reclaim when we can PROVE the owner is dead (pid present and not
          // alive). Files without a pid (older format) are kept and fall through
          // to the health check, since we can't prove they're dead.
          if (info.pid !== undefined && !isPidAlive(info.pid)) {
            this.reclaimDeadPortFile(filePath);
            continue;
          }
          live.push({ ...info, source: "instance", path: filePath });
        }
        // Newest first: files with a started_at sort before those without.
        // Break ties deterministically (equal/absent started_at) by pid then
        // port so selection is stable across reads rather than depending on
        // directory iteration order.
        live.sort(
          (a, b) =>
            (b.started_at ?? 0) - (a.started_at ?? 0) ||
            (b.pid ?? 0) - (a.pid ?? 0) ||
            (b.port ?? 0) - (a.port ?? 0),
        );
        for (const info of live) add(info);
      } catch {
        // ignore read errors
      }
    }

    // Legacy single file (pre-v0.28.2 plugin versions in mixed deployments).
    // Always append it after per-instance entries so a stale JSON file cannot
    // mask an older live server, then de-dupe by port with per-instance winning.
    const legacyInfo = this.parsePortFile(this.legacyPortFile);
    if (legacyInfo) add({ ...legacyInfo, source: "legacy", path: this.legacyPortFile });

    return collected;
  }

  /** Delete a port file whose owning process is provably dead (best-effort). */
  private reclaimDeadPortFile(filePath: string): void {
    try {
      unlinkSync(filePath);
    } catch {
      // best-effort; a concurrent writer may have already replaced it
    }
  }

  private parsePortFile(filePath: string): ParsedPortInfo | null {
    try {
      const content = readFileSync(filePath, "utf-8");
      return parseRpcPortRecord(content);
    } catch {
      return null;
    }
  }

  private portFailureKey(info: PortInfo): string | null {
    if (info.source !== "instance" || !info.path) return null;
    return `${info.path}\0${info.port}\0${info.token ?? ""}`;
  }

  private clearPortFailure(info: PortInfo): void {
    const key = this.portFailureKey(info);
    if (key) this.stalePortFailures.delete(key);
  }

  private recordPortFailure(info: PortInfo): void {
    const key = this.portFailureKey(info);
    if (!key || !info.path) return;

    const failures = (this.stalePortFailures.get(key) ?? 0) + 1;
    if (failures < 2) {
      this.stalePortFailures.set(key, failures);
      return;
    }

    this.stalePortFailures.delete(key);
    // Deletion requires a provably-dead owner. Health checks time out under
    // host load (blocked event loop during builds/streaming), and unlinking a
    // LIVE server's port file is permanent: the server only wrote it at
    // startup, so the sidebar could never reconnect until host restart
    // (issue #110). Live-pid and pid-less files are skipped this round and
    // retried on the next poll instead.
    if (info.pid === undefined || isPidAlive(info.pid)) return;
    try {
      // Do not unlink a replacement written after the failed health checks.
      const current = this.parsePortFile(info.path);
      if (current?.port === info.port && current.token === info.token) {
        unlinkSync(info.path);
      }
    } catch {
      // best-effort stale cleanup only
    }
  }

  private async healthCheck(
    port: number,
    expectedPid: number | undefined,
    signal?: AbortSignal,
  ): Promise<boolean> {
    try {
      return await this.fetchWithTimeout(
        `http://127.0.0.1:${port}/health`,
        { method: "GET" },
        signal,
        async (response) => {
          if (!response.ok) return false;
          // Identity check: the port file records the owning pid and `/health`
          // echoes the live server's pid. If they disagree, the port was recycled
          // by a *different* AFT server (or an unrelated localhost service that
          // happens to answer /health), so this port file is stale — reject it so
          // we don't POST this file's token to the wrong server. Only enforced when
          // both sides expose a pid (older port files omit it).
          if (expectedPid !== undefined) {
            try {
              const body = (await response.json()) as { pid?: unknown };
              if (typeof body?.pid === "number" && body.pid !== expectedPid) return false;
            } catch {
              // Non-JSON / unexpected body: not a healthy AFT server we trust.
              return false;
            }
          }
          if (expectedPid === undefined) await response.body?.cancel();
          return true;
        },
      );
    } catch {
      throwIfAborted(signal);
      return false;
    }
  }

  private async fetchWithTimeout<T>(
    url: string,
    options: RequestInit,
    signal: AbortSignal | undefined,
    consume: (response: Response) => Promise<T>,
  ): Promise<T> {
    throwIfAborted(signal);

    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
    const onAbort = () => controller.abort();
    signal?.addEventListener("abort", onAbort, { once: true });
    try {
      const response = await fetch(url, { ...options, signal: controller.signal });
      return await consume(response);
    } finally {
      clearTimeout(timeout);
      signal?.removeEventListener("abort", onAbort);
    }
  }

  reset(): void {
    this.port = null;
    this.token = null;
    this.stalePortFailures.clear();
  }
}
