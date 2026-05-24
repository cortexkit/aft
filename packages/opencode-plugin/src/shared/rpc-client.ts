import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { warn } from "../logger";
import { rpcPortFileDir, rpcPortFilePath } from "./rpc-utils";

const MAX_RETRIES = 10;
const RETRY_DELAY_MS = 500;
const REQUEST_TIMEOUT_MS = 5000;

type PortInfo = { port: number; token: string | null };

export class AftRpcClient {
  private port: number | null = null;
  private token: string | null = null;
  private portsDir: string;
  private legacyPortFile: string;

  constructor(storageDir: string, directory: string) {
    this.portsDir = rpcPortFileDir(storageDir, directory);
    this.legacyPortFile = rpcPortFilePath(storageDir, directory);
  }

  /** Call an RPC method. Retries port resolution if the server isn't ready yet. */
  async call<T = Record<string, unknown>>(
    method: string,
    params: Record<string, unknown> = {},
  ): Promise<T> {
    // Try ALL discovered ports for this project (OpenCode TUI under --port 0
    // loads our plugin twice in the same process, so two RPC servers listen
    // and we have to try both — only one's bridge is actually warm).
    const infos = await this.resolvePortInfos();
    if (infos.length === 0) {
      throw new Error("AFT RPC server not available");
    }

    // First pass: try every port. Prefer responses that look like "warm
    // bridge" output (i.e. not the synthetic `status: "not_initialized"`
    // placeholder served when this instance's bridge hasn't been spawned).
    let placeholder: T | null = null;
    let lastError: unknown = null;
    for (const info of infos) {
      try {
        const result = await this.callOne<T>(method, params, info);
        if (this.looksLikePlaceholder(result)) {
          placeholder = result; // remember but keep trying
          continue;
        }
        // Warm response — cache this port for subsequent calls.
        this.port = info.port;
        this.token = info.token;
        return result;
      } catch (err) {
        lastError = err;
      }
    }

    // All ports returned placeholder OR failed. Use placeholder if we have
    // one (sidebar then shows the lazy-spawn UI); otherwise rethrow last error.
    if (placeholder !== null) return placeholder;
    throw lastError instanceof Error ? lastError : new Error(String(lastError));
  }

  private async callOne<T>(
    method: string,
    params: Record<string, unknown>,
    info: PortInfo,
  ): Promise<T> {
    const response = await this.fetchWithTimeout(`http://127.0.0.1:${info.port}/rpc/${method}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ ...params, token: info.token }),
    });
    if (!response.ok) {
      const text = await response.text();
      throw new Error(`RPC ${method} failed (${response.status}): ${text}`);
    }
    return (await response.json()) as T;
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

  /** Check if any RPC server is reachable. */
  async isAvailable(): Promise<boolean> {
    try {
      const infos = await this.resolvePortInfos();
      return infos.length > 0;
    } catch {
      return false;
    }
  }

  /**
   * Discover all live RPC port files for this project. Tries the per-instance
   * directory first (v0.28.2+), then falls back to the single legacy `port`
   * file (older plugin versions in mixed deployments).
   */
  private async resolvePortInfos(): Promise<PortInfo[]> {
    for (let attempt = 0; attempt < MAX_RETRIES; attempt++) {
      const infos = this.readAllPortFiles();
      if (infos.length > 0) {
        const alive: PortInfo[] = [];
        for (const info of infos) {
          if (await this.healthCheck(info.port)) {
            alive.push(info);
          }
        }
        if (alive.length > 0) return alive;
      }
      if (attempt < MAX_RETRIES - 1) {
        await new Promise((r) => setTimeout(r, RETRY_DELAY_MS));
      }
    }
    return [];
  }

  private readAllPortFiles(): PortInfo[] {
    const collected: PortInfo[] = [];
    // Per-instance directory (v0.28.2+): one file per plugin load.
    if (existsSync(this.portsDir)) {
      try {
        const entries = readdirSync(this.portsDir);
        for (const entry of entries) {
          if (!entry.endsWith(".json")) continue;
          const info = this.parsePortFile(join(this.portsDir, entry));
          if (info) collected.push(info);
        }
      } catch {
        // ignore read errors
      }
    }
    // Legacy single file (pre-v0.28.2 plugin versions in mixed deployments).
    if (collected.length === 0) {
      const info = this.parsePortFile(this.legacyPortFile);
      if (info) collected.push(info);
    }
    return collected;
  }

  private parsePortFile(path: string): PortInfo | null {
    try {
      const content = readFileSync(path, "utf-8").trim();
      let port: number;
      let token: string | null;
      if (content.startsWith("{")) {
        const parsed = JSON.parse(content) as { port?: unknown; token?: unknown };
        port = typeof parsed.port === "number" ? parsed.port : Number.NaN;
        token = typeof parsed.token === "string" ? parsed.token : null;
      } else {
        warn("RPC port file uses legacy integer format; unauthenticated RPC is deprecated");
        port = Number.parseInt(content, 10);
        token = null;
      }
      if (Number.isNaN(port) || port <= 0 || port > 65535) {
        return null;
      }
      return { port, token };
    } catch {
      return null;
    }
  }

  private async healthCheck(port: number): Promise<boolean> {
    try {
      const response = await this.fetchWithTimeout(`http://127.0.0.1:${port}/health`, {
        method: "GET",
      });
      return response.ok;
    } catch {
      return false;
    }
  }

  private async fetchWithTimeout(url: string, options: RequestInit): Promise<Response> {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
    try {
      return await fetch(url, { ...options, signal: controller.signal });
    } finally {
      clearTimeout(timeout);
    }
  }

  reset(): void {
    this.port = null;
    this.token = null;
  }
}
