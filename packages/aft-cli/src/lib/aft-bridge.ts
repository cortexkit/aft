import { spawn } from "@cortexkit/aft-bridge";
import { CLI } from "./cli.js";

export interface AftRequest {
  id: string;
  command: string;
  [key: string]: unknown;
}

export interface AftResponse {
  id: string;
  success: boolean;
  code?: string;
  message?: string;
  [key: string]: unknown;
}

/**
 * Maximum non-JSON stdout lines we surface in a parse-failure error
 * message. Higher counts just bloat the error output without adding
 * diagnostic value — if the binary is producing pages of garbage, the
 * first 5 lines are enough to tell what kind of binary it is.
 */
const MAX_NOISE_LINES_IN_ERROR = 5;

/**
 * Return true if `parsed` is an aft response keyed by one of the request ids
 * we sent. Push frames (`type: "configure_warnings"`, `type: "progress"`,
 * `type: "bash_completed"`, etc.) have no `id` field and are excluded so the
 * length-based response counter cannot mistake them for the response we want.
 */
function isResponseForRequest(parsed: unknown, expectedIds: Set<string>): boolean {
  if (!parsed || typeof parsed !== "object") return false;
  const obj = parsed as Record<string, unknown>;
  const id = obj.id;
  if (typeof id !== "string") return false;
  return expectedIds.has(id);
}

/**
 * Split an NDJSON byte stream into trimmed lines.
 *
 * Only each new chunk is searched for a newline; the partial line carried
 * between chunks is kept as a list and joined once its newline arrives. The
 * previous `stdout += chunk; stdout.indexOf("\n")` loop rescanned (and, after
 * slicing, recopied) the whole pending line on every chunk, which is quadratic
 * in the size of one large response.
 *
 * `scannedChars` counts the characters searched for newlines so tests can pin
 * the linear cost.
 */
export function createNdjsonLineSplitter(onLine: (line: string) => boolean | undefined): {
  push(chunk: string): void;
  readonly scannedChars: number;
} {
  let pending: string[] = [];
  let scannedChars = 0;
  return {
    push(chunk: string): void {
      scannedChars += chunk.length;
      let start = 0;
      let newline = chunk.indexOf("\n");
      while (newline !== -1) {
        let line = chunk.slice(start, newline);
        if (pending.length > 0) {
          pending.push(line);
          line = pending.join("");
          pending = [];
        }
        start = newline + 1;
        // A true return means the consumer is done; stop splitting.
        if (onLine(line.trim()) === true) return;
        newline = chunk.indexOf("\n", start);
      }
      if (start < chunk.length) pending.push(start === 0 ? chunk : chunk.slice(start));
    },
    get scannedChars() {
      return scannedChars;
    },
  };
}

export async function sendAftRequest(
  binaryPath: string,
  request: AftRequest,
): Promise<AftResponse> {
  const responses = await sendAftRequests(binaryPath, [request]);
  const response = responses[0];
  if (!response) throw new Error("aft exited before responding");
  return response;
}

/**
 * Send NDJSON requests to a long-running `aft` binary and collect
 * matching responses.
 *
 * The contract is forgiving by design: any stdout line that isn't valid
 * JSON is treated as binary noise (panic message, banner from a wrapper
 * script, log line that escaped to stdout, etc.) and remembered for
 * diagnostics rather than crashing the caller. We only report failure
 * when the binary exits without producing the expected number of valid
 * responses — and when we do, the error message names the specific
 * binary path, the noise we observed, and the stderr tail so the user
 * gets actionable context (issue #29 was a raw `SyntaxError` stack from
 * `JSON.parse` on the first leaked stdout line, with no hint what to
 * try next).
 */
export async function sendAftRequests(
  binaryPath: string,
  requests: AftRequest[],
  timeoutMs = 120_000,
): Promise<AftResponse[]> {
  return new Promise((resolve, reject) => {
    const child = spawn(binaryPath, [], {
      stdio: ["pipe", "pipe", "pipe"],
    });
    const responses: AftResponse[] = [];
    // Only the first few noise lines are ever shown; the rest are counted.
    const noiseLines: string[] = [];
    let noiseLineCount = 0;
    let stderr = "";
    let settled = false;
    let timedOut = false;
    const deadline = setTimeout(() => {
      timedOut = true;
      // Rejection waits for close so the child is reaped and stderr has drained.
      child.kill("SIGKILL");
    }, timeoutMs);

    const finish = (fn: () => void): void => {
      if (settled) return;
      settled = true;
      clearTimeout(deadline);
      child.kill();
      fn();
    };

    const expectedIds = new Set(requests.map((req) => req.id));
    const recordNoise = (line: string): void => {
      noiseLineCount += 1;
      if (noiseLines.length < MAX_NOISE_LINES_IN_ERROR) noiseLines.push(line);
    };
    const handleLine = (line: string): void => {
      if (!line) return;
      // Fast-path the protocol: aft writes `{"id":...}` per response.
      // Any other content is binary log noise, panic output, or a
      // wrapper script banner. We swallow it instead of crashing.
      if (!line.startsWith("{")) {
        recordNoise(line);
        return;
      }
      let parsed: unknown;
      try {
        parsed = JSON.parse(line);
      } catch {
        // Looked like JSON but wasn't — also noise.
        recordNoise(line);
        return;
      }
      // Skip push frames (configure_warnings, progress, bash_completed, etc.)
      // and any other unsolicited JSON that isn't a response to one of our
      // requests. These have no `id` field or have an id that doesn't match
      // anything we sent. Issue #34: the configure_warnings frame for missing
      // LSP binaries fired before the lsp_inspect response, so a strict
      // length-based counter mistook it for the inspect response and the CLI
      // reported "lsp_inspect failed" while strace caught the real response
      // mid-write on the wire.
      if (!isResponseForRequest(parsed, expectedIds)) {
        return;
      }
      const response = parsed as AftResponse;
      responses.push(response);
      if (responses.length === requests.length) {
        finish(() => resolve(responses));
      }
    };

    const stdoutLines = createNdjsonLineSplitter((line) => {
      handleLine(line);
      return settled;
    });
    child.stdout.setEncoding("utf-8");
    child.stdout.on("data", (chunk: string) => {
      if (settled || timedOut) return;
      stdoutLines.push(chunk);
    });

    child.stderr.setEncoding("utf-8");
    child.stderr.on("data", (chunk: string) => {
      stderr += chunk;
    });

    child.on("error", (error) => {
      finish(() => reject(error));
    });

    // Listen for "close", not "exit". "exit" fires when the child terminates
    // but stdout/stderr streams may still be flushing buffered chunks. On
    // slow CI runners (observed on macos-latest) the exit handler can fire
    // before the trailing stdout chunks arrive, so noiseLines is incomplete
    // and the resulting error message is missing the binary's actual output.
    // "close" fires only after all stdio streams have closed, guaranteeing
    // every line has been processed by handleLine.
    child.on("close", (code) => {
      if (settled) return;
      if (timedOut) {
        const received = new Set(responses.map((response) => response.id));
        const pending = requests
          .filter((request) => !received.has(request.id))
          .map((request) => request.command);
        finish(() =>
          reject(
            new Error(
              `aft request deadline exceeded; pending: ${pending.join(", ")}.\n${stderr.trim()}`,
              { cause: { exitCode: code, signal: child.signalCode } },
            ),
          ),
        );
        return;
      }
      finish(() =>
        reject(
          buildBridgeError({ binaryPath, code, stderr, noiseLines, noiseLineCount, responses }),
        ),
      );
    });

    // A binary that crashes at startup can close its stdin before (or
    // while) we write the requests. That surfaces as EPIPE/ERR_STREAM_*
    // on the stdin stream - and an un-listened stream error is fatal to
    // the process on newer runtimes. The write failing is not the
    // outcome we report: the "close" handler owns early-exit reporting
    // and produces the actionable error with stderr and noise context.
    child.stdin.on("error", () => {});
    try {
      for (const request of requests) {
        child.stdin.write(`${JSON.stringify(request)}\n`);
      }
      child.stdin.end();
    } catch {
      // Synchronous write/end failure (already-destroyed stream) - same
      // story: let the close handler report the binary's exit.
    }
  });
}

interface BridgeErrorContext {
  binaryPath: string;
  code: number | null;
  stderr: string;
  noiseLines: string[];
  /** Every noise line seen; `noiseLines` keeps only the first few. */
  noiseLineCount: number;
  responses: AftResponse[];
}

function buildBridgeError(ctx: BridgeErrorContext): Error {
  const parts: string[] = [];
  parts.push(
    `aft exited before responding (binary: ${ctx.binaryPath}, exit code: ${ctx.code ?? "unknown"}).`,
  );

  if (ctx.responses.length > 0) {
    parts.push(`Got ${ctx.responses.length} valid response(s) before exit.`);
  }

  if (ctx.noiseLineCount > 0) {
    parts.push(
      `\nThe binary printed ${ctx.noiseLineCount} non-JSON line(s) to stdout — this usually means ` +
        "the resolved binary isn't an AFT release binary (wrapper script, panic output, or unrelated tool):",
    );
    const sample = ctx.noiseLines.slice(0, MAX_NOISE_LINES_IN_ERROR).map((line) => `  | ${line}`);
    parts.push(sample.join("\n"));
    if (ctx.noiseLineCount > MAX_NOISE_LINES_IN_ERROR) {
      parts.push(`  | (… ${ctx.noiseLineCount - MAX_NOISE_LINES_IN_ERROR} more line(s) omitted)`);
    }
    parts.push(
      `\nTry: ${CLI} doctor (full diagnostics) or check ~/.cache/aft/bin/ for the right binary.`,
    );
  }

  const stderrTrimmed = ctx.stderr.trim();
  if (stderrTrimmed) {
    parts.push(`\nstderr:\n${stderrTrimmed}`);
  }

  return new Error(parts.join("\n"));
}
