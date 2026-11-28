/**
 * MCP end-to-end probe: drives a real MCP client conversation (stdio,
 * newline-delimited JSON-RPC) through the subc-mcp shim into a daemon-supervised
 * aft module, and asserts AFT's trust posture for reserved:subc-mcp binds:
 *
 *   1. tools/list exposes the aft tool manifest
 *   2. read INSIDE the project root succeeds (reads work under mcp)
 *   3. read OUTSIDE the project root is DENIED (forced path-restrict)
 *   4. bash is DENIED (bash-family blocked until the sandbox lands)
 *   5. write INSIDE the project root succeeds and mutates disk (writes are day-1)
 *   6. error envelopes are Mode-1 sane (isError:true + text, no thrown garbage)
 *
 * Usage:
 *   bun scripts/mcp-e2e-probe.ts <shim-binary> <module-connection-file> <project-dir>
 */

import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const [shimBin, connFile, projectDir] = process.argv.slice(2);
if (!shimBin || !connFile || !projectDir) {
  console.error("usage: bun mcp-e2e-probe.ts <shim-binary> <module-connection-file> <project-dir>");
  process.exit(2);
}

const child = spawn(shimBin, ["shim", "--module-connection-file", connFile], {
  env: { ...process.env, CLAUDE_PROJECT_DIR: projectDir },
  stdio: ["pipe", "pipe", "inherit"],
});

type JsonRpcMessage = {
  id?: number;
  result?: {
    serverInfo?: unknown;
    tools?: Array<{ name: string }>;
    content?: Array<{ type: string; text?: string }>;
    isError?: boolean;
  };
  error?: { code?: number; message?: string };
};

let buffer = "";
const pending = new Map<number, (msg: JsonRpcMessage) => void>();
child.stdout.on("data", (chunk: Buffer) => {
  buffer += chunk.toString("utf8");
  let idx: number;
  while ((idx = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, idx).trim();
    buffer = buffer.slice(idx + 1);
    if (!line) continue;
    const msg = JSON.parse(line) as JsonRpcMessage;
    if (typeof msg.id === "number" && pending.has(msg.id)) {
      const resolve = pending.get(msg.id);
      pending.delete(msg.id);
      resolve?.(msg);
    }
  }
});

let nextId = 1;
function request(method: string, params?: unknown): Promise<JsonRpcMessage> {
  const id = nextId++;
  const p = new Promise<JsonRpcMessage>((resolve, reject) => {
    pending.set(id, resolve);
    setTimeout(() => {
      if (pending.delete(id)) reject(new Error(`timeout waiting for ${method} (id=${id})`));
    }, 30_000);
  });
  child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
  return p;
}
function notify(method: string, params?: unknown): void {
  child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method, params })}\n`);
}

function callTool(name: string, args: Record<string, unknown>): Promise<JsonRpcMessage> {
  return request("tools/call", { name, arguments: args });
}
function toolText(reply: JsonRpcMessage): string {
  return (reply.result?.content ?? [])
    .filter((c) => c.type === "text")
    .map((c) => c.text ?? "")
    .join("\n");
}

let failures = 0;
function check(label: string, ok: boolean, detail?: string) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures++;
}

const init = await request("initialize", {
  protocolVersion: "2025-06-18",
  capabilities: {},
  clientInfo: { name: "aft-mcp-probe", version: "1.0.0" },
});
check("initialize", !!init.result?.serverInfo, JSON.stringify(init.result?.serverInfo));
notify("notifications/initialized");

const list = await request("tools/list");
const toolNames: string[] = (list.result?.tools ?? []).map((t) => t.name);
check(
  "tools/list exposes aft manifest",
  toolNames.some((n) => n.includes("read")) && toolNames.length >= 20,
  `${toolNames.length} tools`,
);
const name = (suffix: string) =>
  toolNames.find((n) => n === suffix || n.endsWith(`_${suffix}`)) ?? suffix;

// 2. read inside root
const readIn = await callTool(name("read"), { filePath: join(projectDir, "src/greet.ts") });
check(
  "read inside root succeeds",
  readIn.result?.isError !== true && toolText(readIn).includes("hello ${name}"),
);

// 3. read outside root => denied by forced path-restrict
const readOut = await callTool(name("read"), { filePath: "/tmp/aft-mcp-probe-OUTSIDE.txt" });
const outText = toolText(readOut);
check(
  "read outside root DENIED",
  readOut.result?.isError === true || /outside|restrict|denied|path_outside_root/i.test(outText),
  outText.slice(0, 120),
);
check("outside-root content NOT leaked", !outText.includes("secret-outside-root"));

// 4. bash denied for mcp
const bash = await callTool(name("bash"), {
  command: "echo mcp-bash-should-be-denied",
  description: "Run the denied MCP shell probe",
});
const bashText = toolText(bash);
check(
  "bash DENIED for mcp bind",
  bash.result?.isError === true || /denied|not available|untrusted|sandbox/i.test(bashText),
  bashText.slice(0, 120),
);
check("bash did NOT execute", !bashText.includes("mcp-bash-should-be-denied\n"));

// 5. write inside root succeeds + mutates disk
const writePath = join(projectDir, "src/probe-write.ts");
const writeIn = await callTool(name("write"), {
  filePath: writePath,
  content: "export const probe = 42;\n",
});
let wrote = "";
try {
  wrote = readFileSync(writePath, "utf8");
} catch {}
check(
  "write inside root succeeds and mutates disk",
  writeIn.result?.isError !== true && wrote === "export const probe = 42;\n",
);

// 6. envelope sanity on a malformed call
const bad = await callTool(name("read"), {});
check(
  "malformed call returns Mode-1 error envelope (not protocol failure)",
  bad.result?.isError === true && toolText(bad).length > 0,
  toolText(bad).slice(0, 100),
);

child.kill();
console.log(failures === 0 ? "\nALL CHECKS PASSED" : `\n${failures} CHECK(S) FAILED`);
process.exit(failures === 0 ? 0 : 1);
