/// <reference path="../../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { execFile } from "node:child_process";
import { readFile, rm, writeFile } from "node:fs/promises";
import { sep } from "node:path";
import { promisify } from "node:util";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { withHermeticGitEnv } from "../../../../../tests/helpers/git-env.js";
import { buildOpenCodeToolMap, openCodeHashlineEditRegistered } from "../../tool-registration.js";
import type { PluginContext } from "../../types.js";
import { noopAsk } from "../test-helpers.js";
import {
  cleanupHarnesses,
  cleanupSharedSubcRig,
  createHarness,
  type E2EHarness,
  type HarnessFactory,
  type PreparedBinary,
  prepareBinary,
  prepareSubcHarness,
} from "./helpers.js";

process.env.AFT_OPENCODE_E2E_IMPORT_ONLY = "1";
const [
  { runEditWriteToolcallSuite },
  { runReadOnlySpineToolcallSuite },
  { runZoomToolcallSuite },
  { runCallgraphToolcallSuite },
  { runHonestReportingSuite },
  { runApplyPatchRollbackSuite },
  { runFormatOnEditApplyPatchSuite },
  { runSafetySuite },
] = await Promise.all([
  import("./edit-write-toolcall.test.js"),
  import("./read-only-spine-toolcall.test.js"),
  import("./zoom-toolcall.test.js"),
  import("./callgraph-toolcall.test.js"),
  import("./honest-reporting.test.js"),
  import("./apply-patch-rollback.test.js"),
  import("./format-on-edit-apply-patch.test.js"),
  import("./safety.test.js"),
]).finally(() => {
  delete process.env.AFT_OPENCODE_E2E_IMPORT_ONLY;
});

const execFileAsync = promisify(execFile);

const initialBinary = await prepareBinary();
const initialSubc = await prepareSubcHarness(initialBinary);
const skipReason = initialBinary.binaryPath
  ? initialSubc.skipReason
  : (initialBinary.skipReason ?? "aft binary unavailable");
const maybeDescribe = skipReason ? describe.skip : describe;
const describeName = skipReason
  ? `subc transport parity sweep (skipped: ${skipReason})`
  : "subc transport parity sweep";

maybeDescribe(describeName, () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnessFactory: HarnessFactory = (prepared, options) =>
    createHarness(prepared, { ...options, transport: "subc" });

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
    const subc = await prepareSubcHarness(preparedBinary);
    if (subc.skipReason) throw new Error(subc.skipReason);
  }, 30_000);

  afterAll(async () => {
    await cleanupSharedSubcRig();
  });

  runEditWriteToolcallSuite({ harnessFactory, name: "subc parity: edit/write tool_call" });
  runReadOnlySpineToolcallSuite({ harnessFactory, name: "subc parity: read-only spine tool_call" });
  runZoomToolcallSuite({ harnessFactory, name: "subc parity: zoom tool_call" });
  runCallgraphToolcallSuite({ harnessFactory, name: "subc parity: callgraph tool_call" });
  runHonestReportingSuite({ harnessFactory, name: "subc parity: honest reporting" });
  runApplyPatchRollbackSuite({ harnessFactory, name: "subc parity: apply_patch rollback" });
  runFormatOnEditApplyPatchSuite({
    harnessFactory,
    name: "subc parity: format_on_edit apply_patch",
  });
  runSafetySuite({
    harnessFactory,
    name: "subc parity: safety/undo",
  });

  for (const transport of ["ndjson", "subc"] as const) {
    test(`hashline edit patch survives registration and ${transport} dispatch`, async () => {
      const harness = await createHarness(preparedBinary, {
        fixtureNames: [],
        transport,
        useTransportFactoryForNdjson: true,
        tempPrefix: `aft-hashline-plugin-${transport}-`,
        configOverrides: {
          edit_slot_survives: true,
          config: [
            {
              tier: "project",
              source: "/tmp/aft-hashline-plugin.jsonc",
              doc: JSON.stringify({
                edit_mode: "hashline",
                indexes: { trigram: false, semantic: false },
              }),
            },
          ],
        },
      });
      try {
        await writeFile(harness.path("plugin-edit.txt"), "alpha\nbeta\n");
        const sessionID = `hashline-plugin-${transport}`;
        // Initialize the shared project transport under another session first.
        // The tested session must therefore receive its edit-slot flag on its own
        // tool_call instead of inheriting registration from configure.
        const warm = await harness.bridge.toolCall(`${sessionID}-configure-owner`, "read", {
          path: "plugin-edit.txt",
        });
        expect(warm.success).toBe(true);
        const read = await harness.bridge.toolCall(sessionID, "read", { path: "plugin-edit.txt" });
        const tag = read.hashline_tag as string;
        expect(tag).toBeString();

        const config = { edit_mode: "hashline", disabled_tools: [] } as const;
        const pool = { getBridge: () => harness.bridge } as unknown as BridgePool;
        const tools = buildOpenCodeToolMap(
          {
            pool,
            client: {} as PluginContext["client"],
            config,
            hashlineEffective: true,
            storageDir: harness.path(".aft-test-storage"),
          },
          config,
        );
        const toolContext = {
          messageID: `hashline-plugin-${transport}`,
          sessionID,
          agent: "test",
          directory: harness.tempDir,
          worktree: harness.tempDir,
          abort: new AbortController().signal,
          metadata: () => {},
          ask: noopAsk,
        } as ToolContext;
        const patch = `*** Begin Patch\n[plugin-edit.txt#${tag}]\nPUT 1:\n+omega\n*** End Patch`;

        await tools.edit.execute({ patch }, toolContext);

        expect(await readFile(harness.path("plugin-edit.txt"), "utf8")).toBe("omega\nbeta\n");
      } finally {
        await harness.cleanup();
      }
    }, 60_000);
  }

  test("edit registration and Rust argument enforcement agree across transports", async () => {
    const harnesses: E2EHarness[] = [];
    const cases = [
      { label: "default", pluginMode: "default", rustMode: "default" },
      { label: "hashline", pluginMode: "hashline", rustMode: "hashline" },
      { label: "stale plugin config downgrades", pluginMode: "default", rustMode: "hashline" },
    ] as const;

    try {
      for (const transport of ["ndjson", "subc"] as const) {
        for (const testCase of cases) {
          const pluginConfig = { edit_mode: testCase.pluginMode, disabled_tools: [] } as const;
          const surface = buildOpenCodeToolMap(
            {
              pool: {} as PluginContext["pool"],
              client: {} as PluginContext["client"],
              config: pluginConfig,
              hashlineEffective: testCase.pluginMode === "hashline",
              storageDir: "/tmp/aft-hashline-registration",
            },
            pluginConfig,
          );
          const registered = new Set(Object.keys(surface));
          const editSlotSurvives = openCodeHashlineEditRegistered(pluginConfig, registered);
          const harness = await createHarness(preparedBinary, {
            fixtureNames: [],
            transport,
            useTransportFactoryForNdjson: true,
            tempPrefix: `aft-hashline-${transport}-`,
            configOverrides: {
              edit_slot_survives: editSlotSurvives,
              config: [
                {
                  tier: "project",
                  source: "/tmp/aft-hashline-registration.jsonc",
                  doc: JSON.stringify({
                    edit_mode: testCase.rustMode,
                    indexes: { trigram: false, semantic: false },
                  }),
                },
              ],
            },
          });
          harnesses.push(harness);
          await writeFile(harness.path("edit.txt"), "alpha\nbeta\n");
          const configureOwner = await harness.bridge.toolCall(
            `${transport}-${testCase.label}-configure-owner`,
            "read",
            { path: "edit.txt" },
          );
          expect(configureOwner.success).toBe(true);

          const schemaKeys = Object.keys((surface.edit as { args: Record<string, unknown> }).args);
          expect(schemaKeys.includes("patch"), `${transport}/${testCase.label} schema`).toBe(
            testCase.pluginMode === "hashline",
          );

          const session = `${transport}-${testCase.label}`;
          if (testCase.pluginMode === "hashline") {
            const read = await harness.bridge.toolCall(session, "read", { path: "edit.txt" });
            const tag = read.hashline_tag as string;
            expect(tag).toBeString();
            const patch = `*** Begin Patch\n[edit.txt#${tag}]\nPUT 1:\n+omega\n*** End Patch`;
            const applied = await harness.bridge.toolCall(session, "edit", { patch });
            expect(applied.success, `${transport}/${testCase.label}: ${applied.text}`).toBe(true);
            const legacy = await harness.bridge.toolCall(session, "edit", {
              path: "edit.txt",
              edits: [{ oldString: "omega", newString: "legacy" }],
            });
            expect(legacy.code).toBe("hashline_parse_error");
          } else {
            const edited = await harness.bridge.toolCall(session, "edit", {
              path: "edit.txt",
              edits: [{ oldString: "alpha", newString: "default" }],
            });
            expect(edited.success, `${transport}/${testCase.label}: ${edited.text}`).toBe(true);
            if (testCase.pluginMode !== testCase.rustMode) {
              expect(edited.warnings).toEqual([
                {
                  code: "hashline_downgraded",
                  reason: "edit_not_registered",
                  message: expect.any(String),
                },
              ]);
              expect(edited.text).toContain("Hashline mode was downgraded");
            }
            const patch = await harness.bridge.toolCall(session, "edit", {
              patch: "[edit.txt#STALE]\nPUT 1:\n+blocked",
            });
            expect(patch.success).toBe(false);
            if (testCase.pluginMode !== testCase.rustMode) {
              expect(patch.warnings).toBeUndefined();
            }
          }
        }
      }
    } finally {
      await cleanupHarnesses(harnesses);
    }
  }, 120_000);

  test("parity fixture excludes harness state from indexed grep and glob", async () => {
    const harnesses: E2EHarness[] = [];
    try {
      for (const transport of ["ndjson", "subc"] as const) {
        const harness = await createHarness(preparedBinary, {
          fixtureNames: [],
          transport,
          tempPrefix: `aft-plugin-state-${transport}-`,
        });
        harnesses.push(harness);
        await seedParityFixture(harness);
        // Put matching text in the harness's real state directories before the
        // daemon starts, so accidentally indexing them fails without a race.
        await writeFile(
          harness.path(".aft-env", "storage", "parity-probe.txt"),
          "subc_parity_marker\n",
        );
        await writeFile(harness.path(".aft-user", "parity-probe.txt"), "subc_parity_marker\n");

        const ready = (text: string) =>
          !text.includes("[index: building") && !text.includes("[index: fallback]");
        const grep = await toolTextUntil(
          harness,
          "grep",
          { pattern: "subc_parity_marker", path: "." },
          ready,
        );
        expect(ready(grep), `${transport}: ${grep}`).toBe(true);
        expect(grep, transport).toContain("sample.ts");
        expect(grep, transport).not.toContain("parity-probe.txt");
        expect(grep, transport).toContain("Found 1 match in 1 file");

        const glob = await toolTextUntil(harness, "glob", { pattern: "**/*", path: "." }, ready);
        expect(ready(glob), `${transport}: ${glob}`).toBe(true);
        expect(glob, transport).toContain("sample.ts");
        expect(glob, transport).not.toContain(".aft-env/");
        expect(glob, transport).not.toContain(".aft-user/");
      }
    } finally {
      await cleanupHarnesses(harnesses);
    }
  }, 90_000);

  test("standalone indexed grep retires an externally deleted source", async () => {
    const harness = await createHarness(preparedBinary, {
      fixtureNames: [],
      tempPrefix: "aft-plugin-delete-ndjson-",
    });
    try {
      await seedParityFixture(harness);
      await writeFile(harness.path("deleted.ts"), "export const removed = 'subc_parity_marker';\n");
      const args = { pattern: "subc_parity_marker", path: "." };
      const ready = (text: string) =>
        !text.includes("[index: building") && !text.includes("[index: fallback]");
      const before = await toolTextUntil(harness, "grep", args, ready);
      expect(ready(before), before).toBe(true);
      expect(before).toContain("deleted.ts");
      expect(before).toContain("Found 2 matches across 2 files");

      // Delete outside AFT's mutation tools: only the OS watcher can retire
      // this entry, not a tool's direct cache invalidation.
      await rm(harness.path("deleted.ts"));
      const started = Date.now();
      const after = await toolTextUntil(
        harness,
        "grep",
        args,
        (text) => ready(text) && !text.includes("were not on disk in this checkout"),
      );
      expect(after).not.toContain("were not on disk in this checkout");
      expect(after).not.toContain("deleted.ts");
      expect(after).toContain("sample.ts");
      expect(after).toContain("Found 1 match in 1 file");
      // A generous bound for loaded CI runners: the failure this guards is an
      // entry that is never retired, not a slow watcher.
      expect(Date.now() - started).toBeLessThan(30_000);
    } finally {
      await harness.cleanup();
    }
  }, 60_000);

  test("server-rendered text matches NDJSON for representative tool calls", async () => {
    const harnesses: E2EHarness[] = [];
    try {
      const ndjson = await createHarness(preparedBinary, {
        fixtureNames: [],
        timeoutMs: 20_000,
        tempPrefix: "aft-plugin-parity-ndjson-",
      });
      const subc = await harnessFactory(preparedBinary, {
        fixtureNames: [],
        timeoutMs: 20_000,
        tempPrefix: "aft-plugin-parity-subc-",
      });
      harnesses.push(ndjson, subc);
      await Promise.all([seedParityFixture(ndjson), seedParityFixture(subc)]);

      const calls: Array<{ name: string; args: Record<string, unknown> }> = [
        { name: "read", args: { filePath: "sample.ts" } },
        { name: "grep", args: { pattern: "subc_parity_marker", path: "." } },
        { name: "outline", args: { target: "sample.ts" } },
        { name: "zoom", args: { filePath: "sample.ts", symbols: "parityTarget" } },
        { name: "inspect", args: {} },
        { name: "edit", args: { filePath: "edit.txt", oldString: "before", newString: "after" } },
      ];

      for (const call of calls) {
        // Transient index/store states are honest output, not parity gaps —
        // poll BOTH sides to the converged state before comparing. A
        // side that never converges still fails the assertion verbatim.
        // An index that listed a file since deleted is also a transient state:
        // the watcher's next drain removes it, as do watcher changes not yet
        // applied to the index.
        const converged = (text: string) =>
          !text.includes("building/retrying") &&
          !text.includes("[index: building") &&
          !text.includes("were not on disk in this checkout") &&
          !text.includes("Watcher changes pending");
        const ndjsonText = await toolTextUntil(ndjson, call.name, call.args, converged);
        const subcText = await toolTextUntil(subc, call.name, call.args, converged);
        expect(normalizeToolTextForParity(call.name, subcText, subc.tempDir), call.name).toBe(
          normalizeToolTextForParity(call.name, ndjsonText, ndjson.tempDir),
        );
      }
    } finally {
      await cleanupHarnesses(harnesses);
    }
  }, 90_000);
});

async function seedParityFixture(harness: E2EHarness): Promise<void> {
  // The standalone harness puts its child's HOME, XDG directories, storage,
  // and cache under `.aft-env`; subc keeps daemon state outside the project.
  // Both harnesses also write `.aft-user/aft.jsonc`. Exclude these actual
  // state directories so parity compares fixture sources, not one daemon's
  // changing locks, logs, leases, or temporary files.
  await writeFile(harness.path(".aftignore"), ".aft-env/\n.aft-user/\n", "utf8");
  await writeFile(
    harness.path("sample.ts"),
    [
      "export const marker = 'subc_parity_marker';",
      "export function parityTarget(input: string): string {",
      "  return input.trim();",
      "}",
      "// TODO parity inspect marker",
      "",
    ].join("\n"),
    "utf8",
  );
  await writeFile(harness.path("edit.txt"), "before\n", "utf8");
  await gitInitFixture(harness.tempDir);
}

async function gitInitFixture(root: string): Promise<void> {
  const git = (args: string[]) =>
    execFileAsync("git", args, {
      cwd: root,
      env: withHermeticGitEnv(),
    });
  await git(["init"]);
  await git(["config", "user.email", "aft-tests@example.invalid"]);
  await git(["config", "user.name", "AFT Tests"]);
  await git(["add", "."]);
  await git(["commit", "-m", "initial fixture"]);
}

async function toolText(
  harness: E2EHarness,
  name: string,
  args: Record<string, unknown>,
): Promise<string> {
  const response = await harness.bridge.toolCall(`parity-${name}`, name, args);
  expect(response.success, `${name}: ${JSON.stringify(response)}`).toBe(true);
  return response.text;
}

/** toolText, re-polled (1s cadence, 30s budget) until `ready` accepts the
 *  rendered text. Returns the last text either way — a side that never
 *  converges produces a mismatch the assertion reports verbatim. */
async function toolTextUntil(
  harness: E2EHarness,
  name: string,
  args: Record<string, unknown>,
  ready: (text: string) => boolean,
): Promise<string> {
  const deadline = Date.now() + 30_000;
  let text = await toolText(harness, name, args);
  while (!ready(text) && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 1_000));
    text = await toolText(harness, name, args);
  }
  return text;
}

function normalizeToolTextForParity(name: string, text: string, root: string): string {
  const normalized = normalizeRoot(text, root);
  if (name !== "grep") return normalized;
  // The optional ignore probe can finish in one process but not the other.
  // Omit only its trailing grep note; every other output byte stays compared.
  return normalized.replace(/\n\nNot searched \(ignore rules\): [^\r\n]*$/, "");
}

function normalizeRoot(text: string, root: string): string {
  const escapedRoot = root.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const slashRoot = root.split(sep).join("/");
  const escapedSlashRoot = slashRoot.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return (
    text
      .replace(new RegExp(escapedRoot, "g"), "<ROOT>")
      .replace(new RegExp(escapedSlashRoot, "g"), "<ROOT>")
      // The trailing status bar is dropped before comparing. Whether it is
      // appended at all depends on whether it changed since that session last
      // saw it, and its `~` marker on whether a background refresh finished,
      // so it reflects timing and session history, not the transport. Status
      // bar rendering has its own tests.
      .replace(/\n*\[AFT [^\]\n]*\]\s*$/, "")
      // Trailing blank lines went with the bar on one side and not the other.
      .trimEnd()
  );
}
