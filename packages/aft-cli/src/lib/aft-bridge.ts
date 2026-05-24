import { spawn } from "node:child_process";

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
): Promise<AftResponse[]> {
  return new Promise((resolve, reject) => {
    const child = spawn(binaryPath, [], {
      stdio: ["pipe", "pipe", "pipe"],
    });
    const responses: AftResponse[] = [];
    const noiseLines: string[] = [];
    let stdout = "";
    let stderr = "";
    let settled = false;

    const finish = (fn: () => void): void => {
      if (settled) return;
      settled = true;
      child.kill();
      fn();
    };

    const expectedIds = new Set(requests.map((req) => req.id));
    const handleLine = (line: string): void => {
      if (!line) return;
      // Fast-path the protocol: aft writes `{"id":...}` per response.
      // Any other content is binary log noise, panic output, or a
      // wrapper script banner. We swallow it instead of crashing.
      if (!line.startsWith("{")) {
        noiseLines.push(line);
        return;
      }
      let parsed: unknown;
      try {
        parsed = JSON.parse(line);
      } catch {
        // Looked like JSON but wasn't — also noise.
        noiseLines.push(line);
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

    child.stdout.setEncoding("utf-8");
    child.stdout.on("data", (chunk: string) => {
      stdout += chunk;
      while (true) {
        const newline = stdout.indexOf("\n");
        if (newline === -1) break;
        const line = stdout.slice(0, newline).trim();
        stdout = stdout.slice(newline + 1);
        handleLine(line);
        if (settled) break;
      }
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
      finish(() => reject(buildBridgeError({ binaryPath, code, stderr, noiseLines, responses })));
    });

    for (const request of requests) {
      child.stdin.write(`${JSON.stringify(request)}\n`);
    }
    child.stdin.end();
  });
}

interface BridgeErrorContext {
  binaryPath: string;
  code: number | null;
  stderr: string;
  noiseLines: string[];
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

  if (ctx.noiseLines.length > 0) {
    parts.push(
      `\nThe binary printed ${ctx.noiseLines.length} non-JSON line(s) to stdout — this usually means ` +
        "the resolved binary isn't an AFT release binary (wrapper script, panic output, or unrelated tool):",
    );
    const sample = ctx.noiseLines.slice(0, MAX_NOISE_LINES_IN_ERROR).map((line) => `  | ${line}`);
    parts.push(sample.join("\n"));
    if (ctx.noiseLines.length > MAX_NOISE_LINES_IN_ERROR) {
      parts.push(
        `  | (… ${ctx.noiseLines.length - MAX_NOISE_LINES_IN_ERROR} more line(s) omitted)`,
      );
    }
    parts.push(
      "\nTry: npx @cortexkit/aft doctor (full diagnostics) or check ~/.cache/aft/bin/ for the right binary.",
    );
  }

  const stderrTrimmed = ctx.stderr.trim();
  if (stderrTrimmed) {
    parts.push(`\nstderr:\n${stderrTrimmed}`);
  }

  return new Error(parts.join("\n"));
}
