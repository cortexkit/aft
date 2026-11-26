/**
 * Cause-chain and env-proxy diagnostics for built-in `fetch` downloads.
 *
 * Node's built-in `fetch` reports transport errors as `TypeError: fetch failed`
 * with the real reason (`UND_ERR_CONNECT_TIMEOUT`, `ENOTFOUND`, `ECONNREFUSED`,
 * etc.) attached to `error.cause` or `AggregateError.errors`, and ignores
 * `HTTP(S)_PROXY` unless `NODE_USE_ENV_PROXY=1` or `--use-env-proxy` is active.
 */

export interface FormatFetchErrorOptions {
  timedOut?: boolean;
  timeoutMs?: number;
  env?: NodeJS.ProcessEnv;
  isBun?: boolean;
  execArgv?: readonly string[];
}

const ENV_PROXY_FLAG = "--use-env-proxy";

function hasProxyEnv(env: NodeJS.ProcessEnv): boolean {
  for (const key of ["http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY"] as const) {
    if (env[key]?.trim()) return true;
  }
  return false;
}

function isEnvProxyActive(env: NodeJS.ProcessEnv, execArgv: readonly string[]): boolean {
  return (
    env.NODE_USE_ENV_PROXY?.trim() === "1" ||
    execArgv.includes(ENV_PROXY_FLAG) ||
    Boolean(env.NODE_OPTIONS?.split(/\s+/).includes(ENV_PROXY_FLAG))
  );
}

/** Summarize inner cause codes/messages up to depth 4, including AggregateError entries. */
function causeSummary(error: unknown): string {
  const parts: string[] = [];
  const seen = new WeakSet<object>();
  const visit = (value: unknown, depth: number): void => {
    if (depth > 4 || value == null) return;
    if (typeof value === "string") {
      const message = value.trim();
      if (message && message !== "fetch failed") parts.push(message);
      return;
    }
    if (typeof value !== "object" || seen.has(value)) return;
    seen.add(value);
    const record = value as Record<string, unknown>;
    const code = typeof record.code === "string" ? record.code.trim() : "";
    const message = typeof record.message === "string" ? record.message.trim() : "";
    const text =
      code && message && !message.includes(code) ? `${code}: ${message}` : message || code;
    if (text) parts.push(text);
    if (Array.isArray(record.errors)) for (const nested of record.errors) visit(nested, depth + 1);
    visit(record.cause, depth + 1);
  };
  if (typeof error === "object" && error !== null) {
    const record = error as { cause?: unknown; errors?: unknown };
    visit(record.cause, 0);
    if (Array.isArray(record.errors)) for (const nested of record.errors) visit(nested, 0);
  }
  return [...new Set(parts)].join("; ");
}

/** Describe a fetch failure with its timeout, inner cause chain, and Node env-proxy hint. */
export function formatFetchError(error: unknown, opts: FormatFetchErrorOptions = {}): string {
  const base = error instanceof Error ? error.message : String(error);
  let message = opts.timedOut
    ? `timed out after ${Math.round((opts.timeoutMs ?? 0) / 1000)}s`
    : base;
  const cause = causeSummary(error);
  if (cause && !message.includes(cause)) message += ` (${cause})`;

  const env = opts.env ?? process.env;
  const execArgv = opts.execArgv ?? process.execArgv;
  const isBun = opts.isBun ?? Boolean(process.versions.bun);
  const looksLikeTransportFailure =
    Boolean(opts.timedOut) ||
    (typeof error === "object" && error !== null && ("cause" in error || "errors" in error)) ||
    /fetch failed/i.test(base);

  if (!isBun && hasProxyEnv(env) && !isEnvProxyActive(env, execArgv) && looksLikeTransportFailure) {
    message +=
      " (HTTP(S)_PROXY is set, but Node.js fetch ignores it unless NODE_USE_ENV_PROXY=1 is set)";
  }
  return message;
}
