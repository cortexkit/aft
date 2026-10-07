// Build the plugin first, then pass the exact OpenCode 2.0.22 binary as argv[2].
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { join, resolve } from "node:path";
import { isolatedAftEnvironment } from "../../../../aft-bridge/src/test-child-environment.js";

const repo = resolve(import.meta.dir, "../../../../..");
const binary = resolve(process.argv[2]);
const root = mkdtempSync(join(repo, "target", "oc2-tool-surface-"));
const project = join(root, "project");
mkdirSync(project);
const env = { ...process.env };
for (const key of Object.keys(env)) {
  if (key.startsWith("OPENCODE_") || key.startsWith("AFT_") || key.startsWith("XDG_"))
    delete env[key];
}
env.PWD = project;
Object.assign(env, isolatedAftEnvironment(root, env));
assert.equal(Bun.spawnSync(["git", "init", "--quiet"], { cwd: project, env }).exitCode, 0);
const reservation = createServer();
await new Promise<void>((done) => reservation.listen(0, "127.0.0.1", done));
const port = (reservation.address() as { port: number }).port;
await new Promise<void>((done) => reservation.close(() => done()));
const configRoot = join(env.XDG_CONFIG_HOME!, "opencode");
mkdirSync(configRoot);
writeFileSync(join(configRoot, "service.json"), JSON.stringify({ port }));
mkdirSync(join(env.XDG_CONFIG_HOME!, "cortexkit"));
writeFileSync(
  join(env.XDG_CONFIG_HOME!, "cortexkit", "aft.jsonc"),
  JSON.stringify({
    auto_update: false,
    indexes: { semantic: false },
    storage_dir: join(root, "storage"),
  }),
);

interface ModelRequest {
  model: string;
  tools?: Array<{ function: { name: string } }>;
}
const requests: ModelRequest[] = [];
const mock = Bun.serve({
  hostname: "127.0.0.1",
  port: 0,
  async fetch(request) {
    if (request.method !== "POST") return Response.json({ data: [] });
    const body = (await request.json()) as ModelRequest;
    requests.push(body);
    const chunk = (delta: Record<string, unknown>, finishReason: string | null) =>
      `data: ${JSON.stringify({ id: "surface-probe", object: "chat.completion.chunk", created: 0, model: body.model, choices: [{ index: 0, delta, finish_reason: finishReason }] })}\n\n`;
    return new Response(
      chunk({ role: "assistant", content: "Tool surface captured." }, null) +
        chunk({}, "stop") +
        "data: [DONE]\n\n",
      { headers: { "Content-Type": "text/event-stream" } },
    );
  },
});
writeFileSync(
  join(configRoot, "opencode.json"),
  JSON.stringify({
    plugins: [
      join(repo, "packages/opencode-plugin"),
      "-opencode.tool.shell",
      "-opencode.tool.patch",
    ],
    model: "openai/gpt-5-probe",
    snapshots: false,
    permissions: [{ action: "*", resource: "*", effect: "allow" }],
    providers: {
      openai: {
        name: "Isolated tool surface probe",
        package: "@opencode/ai/providers/openai-compatible",
        settings: { baseURL: `http://127.0.0.1:${mock.port}/v1`, apiKey: "probe-only" },
        models: {
          "gpt-5-probe": { name: "Mock GPT" },
          "claude-sonnet-probe": { name: "Mock non-GPT" },
        },
      },
    },
  }),
);
env.OPENCODE_DISABLE_DEFAULT_PLUGINS = "true";
env.AFT_BINARY_PATH = join(repo, "target/debug/aft");
env.OPENAI_API_KEY = "probe-only";
const version = Bun.spawnSync([binary, "--standalone", "--version"], { cwd: project, env });
assert.equal(version.exitCode, 0, version.stderr.toString());
const versionLine = version.stdout.toString().trim();
assert.equal(versionLine, "opencode v2.0.22");
const invocations = [];
const surfaces: Array<{ model: string; tools: string[] }> = [];
console.log(`${versionLine}; probe root: ${root}`);
try {
  for (const id of ["gpt-5-probe", "claude-sonnet-probe"]) {
    const command = [
      binary,
      "run",
      "--standalone",
      "--format",
      "json",
      "--print-logs",
      "--model",
      `openai/${id}`,
      "List the available tools.",
    ];
    invocations.push(command);
    const child = Bun.spawn(command, { cwd: project, env, stdout: "pipe", stderr: "pipe" });
    const deadline = setTimeout(() => child.kill(), 90_000);
    try {
      const [code, stdout, stderr] = await Promise.all([
        child.exited,
        new Response(child.stdout).text(),
        new Response(child.stderr).text(),
      ]);
      writeFileSync(join(root, `${id}.stdout.log`), stdout);
      writeFileSync(join(root, `${id}.stderr.log`), stderr);
      assert.equal(code, 0, stderr);
    } finally {
      clearTimeout(deadline);
    }
    const primary = requests.filter((request) => request.model === id && request.tools?.length);
    assert.equal(primary.length, 1, `expected one real tool-bearing request for ${id}`);
    const names = primary[0].tools!.map((tool) => tool.function.name).sort();
    surfaces.push({ model: id, tools: names });
    assert.ok(names.includes("bash"));
    assert.ok(names.includes("read"));
    assert.ok(!names.includes("shell"), "host shell plugin was not removed");
    assert.ok(!names.includes("patch"), "host patch plugin was not removed");
    assert.equal(names.includes("apply_patch"), id === "gpt-5-probe");
    assert.equal(names.includes("edit"), id !== "gpt-5-probe");
    assert.equal(names.includes("write"), id !== "gpt-5-probe");
    console.log(`${id}: ${names.join(", ")}`);
  }
  console.log(
    "LIVE PASSED: 2 tool-bearing requests; GPT and non-GPT editing surfaces, host patch/shell absent",
  );
} finally {
  mock.stop(true);
  writeFileSync(join(root, "requests.json"), JSON.stringify(requests, null, 2));
  writeFileSync(join(root, "tools.json"), JSON.stringify(surfaces, null, 2));
  writeFileSync(
    join(root, "invocation.json"),
    JSON.stringify(
      {
        version: versionLine,
        commands: invocations,
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
}
