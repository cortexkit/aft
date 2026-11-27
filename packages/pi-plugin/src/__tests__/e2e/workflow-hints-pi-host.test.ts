/// <reference path="../../bun-test.d.ts" />
/**
 * Workflow hints against a real Pi agent session.
 *
 * Uses the Pi pinned by this package unless `AFT_PI_SECTIONS_HOST` points to a
 * directory with a different `@earendil-works/pi-coding-agent` installation.
 * The test skips if that host lacks prompt sections (introduced in Pi 0.86.0).
 *
 * Everything runs in a temporary directory with a scripted faux model, so it
 * never reads or writes the user's Pi configuration and never calls a network
 * provider. The provider callback records the messages of every request,
 * which is the transcript after Pi's own request projections, including the
 * one that collapses all system messages when a handler forces the prompt.
 */

import { afterAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { MAGIC_CONTEXT_SUBAGENT_ENV } from "../../session-kind.js";
import {
  buildHintsFromConfig,
  registerWorkflowHints,
  WORKFLOW_HINTS_SECTION,
} from "../../workflow-hints.js";

// The host's modules are loaded at runtime from a path chosen by the
// environment, so they are typed loosely here.
type AnyModule = any;

interface SystemMessageLike {
  role: "system";
  content: string;
  sections?: Record<string, string | null>;
  toolsAdded?: Array<{ name: string }>;
}

interface PiHost {
  pi: AnyModule;
  ai: AnyModule;
  typebox: AnyModule;
}

async function loadSectionsHost(): Promise<PiHost | null> {
  const from = process.env.AFT_PI_SECTIONS_HOST ?? import.meta.dir;
  try {
    const piEntry = Bun.resolveSync("@earendil-works/pi-coding-agent", from);
    // pi-ai and typebox are resolved from Pi itself so they match its version.
    const piDir = dirname(piEntry);
    const pi = await import(piEntry);
    const ai = await import(Bun.resolveSync("@earendil-works/pi-ai", piDir));
    const typebox = await import(Bun.resolveSync("typebox", piDir));
    // pi-ai's transcript helpers (`getCurrentSystemPrompt`) shipped in 0.86.0
    // together with prompt sections; 0.85.x has neither.
    const sectionsSupported =
      typeof pi.ModelRuntime?.create === "function" &&
      typeof ai.fauxProvider === "function" &&
      typeof ai.getCurrentSystemPrompt === "function";
    return sectionsSupported ? { pi, ai, typebox } : null;
  } catch {
    return null;
  }
}

const host = await loadSectionsHost();
if (!host) {
  console.warn(
    "[workflow-hints-pi-host] skipped: no Pi with prompt sections (>= 0.86.0) resolves; set AFT_PI_SECTIONS_HOST to a directory where one is installed",
  );
}

const HINTS = buildHintsFromConfig({}, new Set()) ?? "";
const RENDERED_SECTION = `<${WORKFLOW_HINTS_SECTION}>\n${HINTS}\n</${WORKFLOW_HINTS_SECTION}>`;
const SURFACE = {
  outline: true,
  zoom: true,
  semantic: true,
  navigate: true,
  inspect: true,
  hoistGrep: true,
  hoistBash: true,
  hoistEdit: true,
  hoistRead: true,
  bashStatus: true,
  bashWatch: true,
  bashWrite: true,
};

const tempDirs: string[] = [];
afterAll(() => {
  for (const dir of tempDirs) rmSync(dir, { recursive: true, force: true });
});

/**
 * Run two user prompts in one session and return the messages of each
 * provider request. The first prompt makes the model call `activate_extra`,
 * which turns on `extra_tool` mid-run, the way lazy tool groups and deferred
 * tools are activated; the second prompt is a plain follow-up turn.
 */
async function runSession({ pi, ai, typebox }: PiHost): Promise<SystemMessageLike[][][]> {
  const dir = mkdtempSync(join(tmpdir(), "aft-pi-hints-"));
  tempDirs.push(dir);
  const agentDir = join(dir, "agent");

  const faux = ai.fauxProvider();
  const runtime = await pi.ModelRuntime.create({
    authPath: join(agentDir, "auth.json"),
    modelsPath: null,
    refreshOnCreate: false,
  });
  runtime.registerNativeProvider(faux.provider);

  const requests: unknown[][] = [];
  const respond = (reply: unknown) => (context: { messages: unknown[] }) => {
    requests.push(structuredClone(context.messages));
    return reply;
  };
  faux.setResponses([
    respond(
      ai.fauxAssistantMessage(ai.fauxToolCall("activate_extra", {}), { stopReason: "toolUse" }),
    ),
    respond(ai.fauxAssistantMessage("activated")),
    respond(ai.fauxAssistantMessage("second turn")),
  ]);

  const extension = (api: AnyModule) => {
    const empty = typebox.Type.Object({});
    api.registerTool({
      name: "activate_extra",
      label: "activate_extra",
      description: "Activate extra_tool.",
      parameters: empty,
      execute: async () => {
        api.setActiveTools([...api.getActiveTools(), "extra_tool"]);
        return { content: [{ type: "text", text: "ok" }] };
      },
    });
    api.registerTool({
      name: "extra_tool",
      label: "extra_tool",
      description: "A tool activated mid-conversation.",
      promptSnippet: "Extra tool",
      parameters: empty,
      execute: async () => ({ content: [{ type: "text", text: "extra" }] }),
    });
    let firstRun = true;
    api.on("before_agent_start", () => {
      // Start without extra_tool so activating it is a mid-run change.
      if (firstRun) api.setActiveTools(["read", "activate_extra"]);
      firstRun = false;
    });
    registerWorkflowHints(api, {}, SURFACE);
  };

  const settingsManager = pi.SettingsManager.inMemory();
  const resourceLoader = new pi.DefaultResourceLoader({
    cwd: dir,
    agentDir,
    settingsManager,
    extensionFactories: [extension],
    noSkills: true,
    noPromptTemplates: true,
    noThemes: true,
    noContextFiles: true,
  });
  await resourceLoader.reload();
  const { session } = await pi.createAgentSession({
    cwd: dir,
    agentDir,
    modelRuntime: runtime,
    model: faux.getModel(),
    tools: ["read", "activate_extra", "extra_tool"],
    resourceLoader,
    sessionManager: pi.SessionManager.inMemory(dir),
    settingsManager,
  });
  try {
    await session.prompt("activate the extra tool");
    await session.prompt("and again");
  } finally {
    session.dispose();
  }
  return requests.map((messages) => [
    messages as SystemMessageLike[],
    (messages as SystemMessageLike[]).filter((message) => message.role === "system"),
  ]);
}

describe.skipIf(!host)("Pi workflow hints on a host with prompt sections", () => {
  test("hints ride as a stable section and mid-run tool changes stay deltas", async () => {
    const previous = process.env[MAGIC_CONTEXT_SUBAGENT_ENV];
    delete process.env[MAGIC_CONTEXT_SUBAGENT_ENV];
    let requests: SystemMessageLike[][][];
    try {
      requests = await runSession(host as PiHost);
    } finally {
      if (previous !== undefined) process.env[MAGIC_CONTEXT_SUBAGENT_ENV] = previous;
    }
    expect(requests).toHaveLength(3);
    const [first, afterActivation, secondTurn] = requests.map(([, system]) => system);

    // Not forced: the head system message is the structured prompt (empty
    // content, named sections). A forced prompt would arrive as one collapsed
    // head holding the whole text in `content` with no sections.
    for (const system of [first, afterActivation, secondTurn]) {
      expect(system[0]?.content).toBe("");
      expect(system[0]?.sections?.[WORKFLOW_HINTS_SECTION]).toBe(RENDERED_SECTION);
    }

    // The rendered system prompt carries the hint text byte-for-byte.
    for (const [messages] of requests) {
      expect((host as PiHost).ai.getCurrentSystemPrompt(messages)).toContain(RENDERED_SECTION);
    }

    // The mid-run activation reaches the request as its own delta system
    // message instead of a rewritten head.
    const deltas = afterActivation.slice(1);
    expect(deltas.flatMap((message) => message.toolsAdded?.map((tool) => tool.name) ?? [])).toEqual(
      ["extra_tool"],
    );
    expect(first[0]?.toolsAdded?.map((tool) => tool.name)).not.toContain("extra_tool");

    // Cache stability: the section never changes, so no later system message
    // patches it and every request carries the same bytes.
    for (const system of [afterActivation, secondTurn]) {
      for (const message of system.slice(1)) {
        expect(message.sections ?? {}).not.toHaveProperty(WORKFLOW_HINTS_SECTION);
      }
    }
  }, 60_000);
});
