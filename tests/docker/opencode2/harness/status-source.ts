import { readFile } from "node:fs/promises";
import { randomUUID } from "node:crypto";

import { AftRpcClient } from "../../../../packages/opencode-plugin/src/shared/rpc-client.js";

/** Query the existing AFT process via V1 status RPC or the V2 loopback HTTP relay. */
export function scenarioStatusSource(options: {
  hostGeneration: "v1" | "v2";
  projectRoot: string;
  storageDir: string;
  endpointFile: string;
  token: string;
}): (remainingMs: number) => Promise<unknown> {
  const legacy = new AftRpcClient(options.storageDir, options.projectRoot);
  return async (remainingMs) => {
    const signal = AbortSignal.timeout(Math.max(1, Math.min(5_000, remainingMs)));
    if (options.hostGeneration === "v1") {
      return legacy.call("status", { sessionID: `readiness-${randomUUID()}` }, { signal });
    }
    const endpoint = new URL((await readFile(options.endpointFile, "utf8")).trim());
    if (endpoint.protocol !== "http:" || endpoint.hostname !== "127.0.0.1") {
      throw new Error("readiness status endpoint is not loopback HTTP");
    }
    const response = await fetch(endpoint, {
      headers: { authorization: `Bearer ${options.token}` },
      signal,
    });
    if (!response.ok) throw new Error(`status relay returned HTTP ${response.status}`);
    return response.json();
  };
}

/**
 * Standalone V2 runs use a private stdio host, not a discoverable HTTP service.
 * Relay only its registered AFT getStatus handler; all tool calls still use the
 * host's own registration and execution path.
 */
export function statusRelayModule(options: {
  pluginEntry: string;
  endpointFile: string;
  token: string;
}): string {
  return `
import { createServer } from "node:http";
import { writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { randomUUID, timingSafeEqual } from "node:crypto";
const { Effect } = createRequire(${JSON.stringify(options.pluginEntry)})("effect");
const token = Buffer.from(${JSON.stringify(`Bearer ${options.token}`)});
export function statusContext(context) {
  return {
    ...context,
    rpc: {
      ...context.rpc,
      register(definition, handlers) {
        if (definition.id === "aft" && handlers.getStatus) {
          const server = createServer(async (request, response) => {
            const supplied = Buffer.from(request.headers.authorization ?? "");
            if (supplied.length !== token.length || !timingSafeEqual(supplied, token)) {
              response.writeHead(401).end();
              return;
            }
            if (request.method !== "GET" || request.url !== "/status") {
              response.writeHead(404).end();
              return;
            }
            try {
              // A fresh session label bypasses the UI's session-specific cache.
              const status = await Effect.runPromise(handlers.getStatus({
                sessionID: "readiness-" + randomUUID(),
              }));
              response.writeHead(200, { "content-type": "application/json" });
              response.end(JSON.stringify(status));
            } catch (error) {
              response.writeHead(200, { "content-type": "application/json" });
              response.end(JSON.stringify({ success: false, status: "unavailable", message: String(error) }));
            }
          });
          server.listen(0, "127.0.0.1", () => {
            writeFileSync(${JSON.stringify(options.endpointFile)},
              "http://127.0.0.1:" + server.address().port + "/status", { mode: 0o600 });
            server.unref();
          });
        }
        return context.rpc.register(definition, handlers);
      },
    },
  };
}
`;
}
