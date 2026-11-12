/**
 * A real OpenCode 2 host with AFT loaded and a scripted model, for the e2e
 * tests that need to see what the host offers the model and what a tool call
 * returns.
 *
 * The host runs as `serve --service` under throwaway HOME, XDG roots,
 * database and service port, so it never reaches the operator's own OpenCode
 * service. AFT is loaded from this package's `dist/` (run `bun run build`
 * first) with the `aft` binary in AFT_BINARY_PATH, default target/debug/aft.
 */

import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { OpenCode, type V2Event } from "@opencode/client";

export const pluginRoot = resolve(import.meta.dir, "../../..");
const repoRoot = resolve(pluginRoot, "../..");
export const aftBinary =
  process.env.AFT_BINARY_PATH?.trim() ||
  join(repoRoot, "target", "debug", process.platform === "win32" ? "aft.exe" : "aft");

type Content = string | Array<{ type: string; text?: string }> | null | undefined;
export type ModelMessage = {
  role: string;
  content?: Content;
  tool_calls?: Array<{ id: string; function: { name: string; arguments: string } }>;
  tool_call_id?: string;
};
export type ModelTool = { function: { name: string; description: string } };
export type ModelRequest = { model: string; messages: ModelMessage[]; tools?: ModelTool[] };

/** A tool call the scripted model makes when a user turn's text is the key. */
export type ModelScript = Record<string, { name: string; arguments: Record<string, unknown> }>;

export function messageText(content: Content): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content.map((part) => part.text ?? "").join("");
}

export async function until(label: string, predicate: () => Promise<boolean>, ms = 60_000) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await Bun.sleep(100);
  }
  throw new Error(`timed out waiting for ${label}`);
}

/**
 * The scripted model: a user turn whose text is a script key gets that tool
 * call (when the tool is offered); anything else gets a plain reply. Every
 * request is recorded, and so is the command of every call it issues.
 */
function startModel(
  script: ModelScript,
  requests: ModelRequest[],
  issued: string[],
  errors: V2HostOptions["modelErrors"],
) {
  let call = 0;
  return Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request) {
      if (request.method !== "POST") return Response.json({ data: [] });
      const body = (await request.json()) as ModelRequest;
      requests.push(body);
      const chunk = (delta: Record<string, unknown>, finish: string | null) =>
        `data: ${JSON.stringify({
          id: "scripted",
          object: "chat.completion.chunk",
          created: 1,
          model: body.model,
          choices: [{ index: 0, delta, finish_reason: finish }],
        })}\n\n`;
      const last = body.messages.at(-1);
      const error = last?.role === "user" ? errors?.[messageText(last.content).trim()] : undefined;
      if (error)
        return Response.json(
          { error: { message: error.message, type: error.code, code: error.code } },
          { status: error.status },
        );
      const step = last?.role === "user" ? script[messageText(last.content).trim()] : undefined;
      if (step && body.tools?.some((tool) => tool.function.name === step.name)) {
        call += 1;
        issued.push(String(step.arguments.command));
        return new Response(
          chunk(
            {
              role: "assistant",
              tool_calls: [
                {
                  index: 0,
                  id: `call_${call}`,
                  type: "function",
                  function: { name: step.name, arguments: JSON.stringify(step.arguments) },
                },
              ],
            },
            null,
          ) +
            chunk({}, "tool_calls") +
            "data: [DONE]\n\n",
          { headers: { "content-type": "text/event-stream" } },
        );
      }
      return new Response(
        chunk({ role: "assistant", content: "Done." }, null) +
          chunk({}, "stop") +
          "data: [DONE]\n\n",
        { headers: { "content-type": "text/event-stream" } },
      );
    },
  });
}

/** The result the host sent back to the model for the call that ran `command`. */
export function toolResult(requests: ModelRequest[], command: string): string | undefined {
  for (const request of requests) {
    const calls = new Map<string, string>();
    for (const message of request.messages) {
      for (const call of message.tool_calls ?? []) calls.set(call.id, call.function.arguments);
      if (message.role === "tool" && message.tool_call_id) {
        const args = calls.get(message.tool_call_id);
        if (args && (JSON.parse(args) as { command?: string }).command === command) {
          return messageText(message.content);
        }
      }
    }
  }
  return undefined;
}

export function taskStatusResult(requests: ModelRequest[], taskId: string): string | undefined {
  for (const request of [...requests].reverse()) {
    const calls = new Set(
      request.messages.flatMap((message) =>
        (message.tool_calls ?? [])
          .filter(
            (call) =>
              call.function.name === "bash_status" &&
              JSON.parse(call.function.arguments).taskId === taskId,
          )
          .map((call) => call.id),
      ),
    );
    const result = [...request.messages]
      .reverse()
      .find(
        (message) =>
          message.role === "tool" && message.tool_call_id && calls.has(message.tool_call_id),
      );
    if (result) return messageText(result.content);
  }
}

export interface V2HostOptions {
  label: string;
  script: ModelScript;
  /** Add the user config entries that remove the host's own shell and patch plugins. */
  removeBuiltins?: boolean;
  /** Host permission rules after the leading allow-everything rule. */
  permissions?: Array<{ action: string; resource: string; effect: string }>;
  /** Files written into the project before the host starts. */
  files?: Record<string, string>;
  /** User-tier AFT config, written under the host's own XDG_CONFIG_HOME. */
  userAftConfig?: Record<string, unknown>;
  /** Use XDG storage resolution instead of an explicit AFT_STORAGE_DIR override. */
  defaultAftStorage?: boolean;
  modelErrors?: Record<string, { status: number; code: string; message: string }>;
}

export async function bootV2Host(hostBinary: string, options: V2HostOptions) {
  const tempParent = resolve(pluginRoot, "tmp");
  await mkdir(tempParent, { recursive: true });
  const root = await mkdtemp(join(tempParent, `v2-host-${options.label}-`));
  const project = join(root, "project");
  const config = join(root, "config", "opencode");
  const dirs = Object.fromEntries(
    ["home", "config", "data", "state", "cache", "temp", "aft-storage", "aft-cache"].map((name) => [
      name,
      join(root, name),
    ]),
  );
  await Promise.all(
    [...Object.values(dirs), project, config, join(project, ".cortexkit")].map((path) =>
      mkdir(path, { recursive: true }),
    ),
  );
  const git = spawnSync("git", ["init", "-q"], { cwd: project, encoding: "utf8" });
  if (git.status !== 0) throw new Error(`could not initialize the project: ${git.stderr}`);
  if (options.userAftConfig) {
    await mkdir(join(dirs.config, "cortexkit"), { recursive: true });
    await writeFile(
      join(dirs.config, "cortexkit", "aft.jsonc"),
      JSON.stringify(options.userAftConfig),
    );
  }
  for (const [name, contents] of Object.entries(options.files ?? {})) {
    await writeFile(join(project, name), contents);
  }
  await writeFile(
    join(project, ".cortexkit", "aft.jsonc"),
    JSON.stringify({
      indexes: { trigram: false, semantic: false },
      disabled_tools: [],
      bash: true,
      lsp: { auto_install: false },
    }),
  );

  const requests: ModelRequest[] = [];
  const issued: string[] = [];
  const model = startModel(options.script, requests, issued, options.modelErrors);
  const eventAbort = new AbortController();
  const events: V2Event[] = [];

  // An explicit service port plus throwaway HOME/XDG roots and database keep
  // the host off the operator's managed OpenCode service.
  const portProbe = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => new Response("") });
  const port = portProbe.port;
  portProbe.stop(true);
  await writeFile(join(config, "service.json"), JSON.stringify({ hostname: "127.0.0.1", port }));
  await writeFile(
    join(config, "opencode.json"),
    JSON.stringify({
      plugins: [
        pluginRoot,
        ...(options.removeBuiltins ? ["-opencode.tool.shell", "-opencode.tool.patch"] : []),
      ],
      model: "scripted/gpt-5-scripted",
      snapshots: false,
      permissions: [
        { action: "*", resource: "*", effect: "allow" },
        ...(options.permissions ?? []),
      ],
      providers: {
        scripted: {
          name: "Scripted model",
          package: "@opencode/ai/providers/openai-compatible",
          settings: { baseURL: `${model.url}v1`, apiKey: "test-only" },
          models: { "gpt-5-scripted": { name: "Mock GPT" } },
        },
      },
    }),
  );

  let logs = "";
  const child: ChildProcess = spawn(hostBinary, ["serve", "--service", "--print-logs"], {
    cwd: project,
    env: {
      PATH: process.env.PATH,
      HOME: dirs.home,
      OPENCODE_TEST_HOME: dirs.home,
      XDG_CONFIG_HOME: dirs.config,
      XDG_DATA_HOME: dirs.data,
      XDG_STATE_HOME: dirs.state,
      XDG_CACHE_HOME: dirs.cache,
      TMPDIR: dirs.temp,
      NODE_ENV: "test",
      OPENCODE_DISABLE_DEFAULT_PLUGINS: "true",
      OPENCODE_DISABLE_MODELS_FETCH: "true",
      OPENCODE_DB: `v2-host-${options.label}.db`,
      AFT_BINARY_PATH: aftBinary,
      ...(options.defaultAftStorage ? {} : { AFT_STORAGE_DIR: dirs["aft-storage"] }),
      AFT_CACHE_DIR: dirs["aft-cache"],
      // AFT's own log goes to stderr, and so into a failure's host output.
      AFT_LOG_STDERR: "1",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout?.on("data", (chunk) => {
    logs += String(chunk);
  });
  child.stderr?.on("data", (chunk) => {
    logs += String(chunk);
  });
  const stop = async () => {
    child.kill("SIGTERM");
    await new Promise<void>((done) => {
      if (child.exitCode !== null) return done();
      child.once("exit", () => done());
      setTimeout(() => child.kill("SIGKILL"), 2000).unref();
    });
    model.stop(true);
    eventAbort.abort();
    await rm(root, { recursive: true, force: true });
  };

  try {
    let password: string | undefined;
    await until("the host service registration", async () => {
      if (child.exitCode !== null) throw new Error(`V2 host exited: ${logs}`);
      const state = await readFile(join(dirs.state, "opencode", "service.json"), "utf8").catch(
        () => undefined,
      );
      if (!state) return false;
      password = JSON.parse(state).password;
      return true;
    });
    const headers: Record<string, string> =
      password === undefined
        ? {}
        : { authorization: `Basic ${Buffer.from(`opencode:${password}`).toString("base64")}` };
    await until("the host API", async () => {
      if (child.exitCode !== null) throw new Error(`V2 host exited: ${logs}`);
      return fetch(`http://127.0.0.1:${port}/api/info`, { headers })
        .then((response) => response.ok)
        .catch(() => false);
    });
    const info = (await fetch(`http://127.0.0.1:${port}/api/info`, { headers }).then((response) =>
      response.json(),
    )) as { version: string };
    const client = OpenCode.make({ baseUrl: `http://127.0.0.1:${port}`, headers });
    const subscription = client.event
      .subscribe({ signal: eventAbort.signal })
      [Symbol.asyncIterator]();
    const first = subscription.next();
    void (async () => {
      let next = await first;
      while (!next.done) {
        events.push(next.value);
        next = await subscription.next();
      }
    })().catch(() => {});
    await client.location.get({ location: { directory: project } });
    return {
      client,
      project,
      requests,
      issued,
      version: info.version,
      events,

      logs: () => logs,
      stop,
      async newSession(title: string) {
        return client.session.create({ title, location: { directory: project } });
      },
      /** Send a prompt and wait until the session is idle again. */
      async turn(sessionID: string, text: string) {
        await client.session.prompt({ sessionID, text });
        await client.session.wait({ sessionID });
      },
    };
  } catch (error) {
    await stop();
    throw error;
  }
}

export type V2Host = Awaited<ReturnType<typeof bootV2Host>>;
