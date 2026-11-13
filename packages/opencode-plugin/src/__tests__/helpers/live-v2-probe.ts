// Run with Bun after building the plugin; pass an exact V2 binary as argv[2].
import { Database } from "bun:sqlite";
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { join, resolve } from "node:path";

const repo = resolve(import.meta.dir, "../../../../..");
const binary = resolve(process.argv[2]);
const root = mkdtempSync(join(repo, "target", "oc2-live-"));
const project = join(root, "project");
mkdirSync(project);
const env = { ...process.env };
env.PWD = project;
for (const key of Object.keys(env)) if (key.startsWith("OPENCODE_")) delete env[key];
for (const key of [
  "HOME",
  "XDG_CONFIG_HOME",
  "XDG_DATA_HOME",
  "XDG_STATE_HOME",
  "XDG_CACHE_HOME",
  "XDG_RUNTIME_DIR",
]) {
  env[key] = join(root, key.toLowerCase());
  mkdirSync(env[key]!);
}
assert.equal(Bun.spawnSync(["git", "init", "--quiet"], { cwd: project, env }).exitCode, 0);
const reservation = createServer();
await new Promise<void>((done) => reservation.listen(0, "127.0.0.1", done));
const port = (reservation.address() as { port: number }).port;
await new Promise<void>((done) => reservation.close(() => done()));
mkdirSync(join(env.XDG_CONFIG_HOME!, "opencode"));
writeFileSync(join(env.XDG_CONFIG_HOME!, "opencode", "service.json"), JSON.stringify({ port }));
mkdirSync(join(env.XDG_CONFIG_HOME!, "cortexkit"));
writeFileSync(
  join(env.XDG_CONFIG_HOME!, "cortexkit", "aft.jsonc"),
  JSON.stringify({
    restrict_to_project_root: true,
    auto_update: false,
    indexes: { semantic: false },
    storage_dir: join(root, "storage"),
  }),
);
writeFileSync(join(project, "inside.txt"), "LIVE_ALIAS_READ_OK\n");
const outside = join(root, "outside.txt");
writeFileSync(outside, "not accessible\n");

const requests: any[] = [];
let turn = 0;
const mock = Bun.serve({
  hostname: "127.0.0.1",
  port: 0,
  async fetch(request) {
    if (request.method !== "POST") return Response.json({ data: [] });
    const body = (await request.json()) as any;
    const primary = Array.isArray(body.tools) && body.tools.length > 0;
    if (primary) requests.push(body);
    const calls = [
      { name: "read", arguments: { filePath: outside } },
      {
        name: "bash",
        arguments: {
          command:
            "printf 'CONFLICT (content): file.ts\\nAutomatic merge failed; fix conflicts and then commit the result.\\n'",
        },
      },
      { name: "read", arguments: { filePath: "inside.txt" } },
    ];
    const call = primary ? calls[turn++] : undefined;
    const delta = call
      ? {
          role: "assistant",
          tool_calls: [
            {
              index: 0,
              id: `call-${turn}`,
              type: "function",
              function: { name: call.name, arguments: JSON.stringify(call.arguments) },
            },
          ],
        }
      : { role: "assistant", content: "Probe complete" };
    const chunk = (value: any) =>
      `data: ${JSON.stringify({ id: "probe", object: "chat.completion.chunk", created: 0, model: "gpt-5-probe", ...value })}\n\n`;
    return new Response(
      chunk({ choices: [{ index: 0, delta, finish_reason: null }] }) +
        chunk({ choices: [{ index: 0, delta: {}, finish_reason: call ? "tool_calls" : "stop" }] }) +
        "data: [DONE]\n\n",
      { headers: { "Content-Type": "text/event-stream" } },
    );
  },
});

const wrapper = join(root, "plugin");
mkdirSync(wrapper);
const receipts = join(root, "synthetic-receipts.jsonl");
writeFileSync(
  join(wrapper, "server.js"),
  `
import plugin from ${JSON.stringify(join(repo, "packages/opencode-plugin/dist/entry/server.js"))};
import { Effect } from ${JSON.stringify(import.meta.resolve("effect"))};
import { appendFileSync } from "node:fs";
export default { id: "aft-live-probe", effect: (ctx) => plugin.effect({ ...ctx, session: { ...ctx.session, synthetic: (input) => ctx.session.synthetic(input).pipe(Effect.tap(() => Effect.sync(() => appendFileSync(${JSON.stringify(receipts)}, JSON.stringify(input) + "\\n")))) } }) };
`,
);
writeFileSync(
  join(env.XDG_CONFIG_HOME!, "opencode", "opencode.json"),
  JSON.stringify({
    plugins: [wrapper],
    model: "openai/gpt-5-probe",
    snapshots: false,
    permissions: [{ action: "*", resource: "*", effect: "allow" }],
    providers: {
      openai: {
        name: "Isolated probe",
        package: "@opencode/ai/providers/openai-compatible",
        settings: { baseURL: `http://127.0.0.1:${mock.port}/v1`, apiKey: "probe-only" },
        models: { "gpt-5-probe": { name: "Mock model" } },
      },
    },
  }),
);
env.OPENCODE_DISABLE_DEFAULT_PLUGINS = "true";
env.AFT_BINARY_PATH = join(repo, "target/debug/aft");
env.OPENAI_API_KEY = "probe-only";
const command = [
  binary,
  "run",
  "--standalone",
  "--format",
  "json",
  "--print-logs",
  "--model",
  "openai/gpt-5-probe",
  "Run the probe",
];
writeFileSync(
  join(root, "invocation.json"),
  JSON.stringify(
    {
      command,
      project,
      roots: Object.fromEntries(
        Object.entries(env).filter(([key]) => key === "HOME" || key.startsWith("XDG_")),
      ),
      port,
    },
    null,
    2,
  ),
);
console.log(`Probe root: ${root}`);
const child = Bun.spawn(command, { cwd: project, env, stdout: "pipe", stderr: "pipe" });
const deadline = setTimeout(() => child.kill(), 90_000);
try {
  const [code, stdout, stderr] = await Promise.all([
    child.exited,
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
  ]);
  writeFileSync(join(root, "stdout.log"), stdout);
  writeFileSync(join(root, "stderr.log"), stderr);
  writeFileSync(join(root, "requests.json"), JSON.stringify(requests, null, 2));
  assert.equal(code, 0, stderr);
  assert.equal(requests.length, 4);
  assert.ok(JSON.stringify(requests[0].messages).includes("# Agent Foundational Toolkit"));
  const names = requests[0].tools.map((entry: any) => entry.function.name);
  for (const name of ["shell", "bash", "patch", "apply_patch"])
    assert.ok(names.includes(name), `${name} not offered`);
  assert.ok(JSON.stringify(requests[2].messages).includes("[Hint] Use aft_conflicts"));
  assert.ok(JSON.stringify(requests[3].messages).includes("LIVE_ALIAS_READ_OK"));
  const notice = readFileSync(receipts, "utf8");
  assert.ok(notice.includes("AFT blocked access to a path outside the project"));
  assert.ok(notice.includes('"resume":false'));
  const db = new Database(join(env.XDG_DATA_HOME!, "opencode", "opencode.db"), { readonly: true });
  try {
    const messages = db.query("SELECT * FROM session_message").all();
    assert.ok(
      JSON.stringify(messages).includes("AFT blocked access to a path outside the project"),
    );
  } finally {
    db.close();
  }
  console.log(
    "LIVE PASSED: 4 model requests; synthetic notice persisted without resume, aliases accepted, bash conflicts hint, workflow guidance, shell/bash and patch/apply_patch coexist",
  );
} finally {
  clearTimeout(deadline);
  mock.stop(true);
}
