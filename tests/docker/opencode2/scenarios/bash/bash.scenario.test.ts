import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import type { HostEvent } from "../../harness/event-stream.js";
import {
  assertBashAftExecutionIdentity,
  assertBashFallbackAskIdentity,
  assertConfigDenyHidesTool,
  HOST_FALLBACK_ASK_MARKER,
  HOST_FALLBACK_OUTPUT_MARKER,
} from "../../harness/permission-plan.js";
import { loadScenarios, materializeParityScenarios } from "../../harness/scenario-loader.js";
import type { RecordedMockExchange, ScenarioDefinition } from "../../harness/types.js";
import extension from "./bash.extension.js";

const loaded = materializeParityScenarios(await loadScenarios(resolve(import.meta.dir)));
const scenarios = new Map(loaded.map((scenario) => [scenario.id, scenario]));

function scenario(id: string): ScenarioDefinition {
  const found = scenarios.get(id);
  if (!found) throw new Error(`scenario not registered: ${id}`);
  return found;
}

/** A model request as the host builds it, offering the named tools. */
function exchange(label: string, offeredTools: readonly string[]): RecordedMockExchange {
  return {
    index: 0,
    label,
    request: {
      tools: offeredTools.map((name) => ({ type: "function", function: { name } })),
    },
    response: {},
    observed_at: "1970-01-01T00:00:00.000Z",
  };
}

function askedFor(text: string): HostEvent {
  return { type: "permission.asked", data: { action: "bash", resources: [text] } };
}

describe("bash OpenCode 2 scenarios", () => {
  test("load through the harness loader and satisfy the slice validator", async () => {
    expect(loaded.length).toBe(15);
    await extension.validate?.({
      repo_root: resolve(import.meta.dir, "../../../../../.."),
      platform: "linux",
      scenarios: loaded,
      matrix: {},
      pinned_host_version: "2.0.3",
    });
  });
});

describe("a configured denial hides the tool it names", () => {
  const denied = scenario("bash/T3/loop_config_deny");

  test("accepts a tool the host offered before the rule and dropped after it", () => {
    expect(() =>
      assertConfigDenyHidesTool(denied, [
        exchange("call-tool", ["bash", "read", "shell"]),
        exchange("finish", ["read", "shell"]),
      ]),
    ).not.toThrow();
  });

  test("rejects a tool the host never offered, which never registered rather than being hidden", () => {
    expect(() =>
      assertConfigDenyHidesTool(denied, [
        exchange("call-tool", ["read", "shell"]),
        exchange("finish", ["read", "shell"]),
      ]),
    ).toThrow("the host never offered bash");
  });

  test("rejects a tool the host kept offering under the rule", () => {
    expect(() =>
      assertConfigDenyHidesTool(denied, [
        exchange("call-tool", ["bash", "read"]),
        exchange("finish", ["bash", "read"]),
      ]),
    ).toThrow("kept offering bash");
  });

  test("rejects a run that never reached the model", () => {
    expect(() => assertConfigDenyHidesTool(denied, [])).toThrow("offered no tools");
  });

  test("says nothing about rows whose permission is answered rather than configured", () => {
    expect(() =>
      assertConfigDenyHidesTool(scenario("bash/T3/loop_ask_deny"), [
        exchange("call-tool", ["read"]),
      ]),
    ).not.toThrow();
  });
});

describe("the break-glass ask belongs to the fallback path alone", () => {
  test("a loop row that fell back to host execution is the wrong path", () => {
    expect(() =>
      assertBashFallbackAskIdentity(scenario("bash/T3/loop_ask_allow"), {
        resultText: `${HOST_FALLBACK_OUTPUT_MARKER} - module transport down]\nok`,
        events: [],
      }),
    ).toThrow("fell back to host execution");
  });

  test("a rule-denied row that raised the break-glass ask found a way around the denial", () => {
    expect(() =>
      assertBashFallbackAskIdentity(scenario("bash/T3/fallback_config_deny"), {
        resultText: "",
        events: [askedFor(`${HOST_FALLBACK_ASK_MARKER} (transport down) - host fallback`)],
      }),
    ).toThrow("raised the host-fallback ask");
  });

  test("a refused fallback row still has to reach the ask it refuses", () => {
    expect(() =>
      assertBashFallbackAskIdentity(scenario("bash/T3/fallback_ask_deny"), {
        resultText: "[aft-bridge] Binary crashed (restarts: 0)",
        events: [],
      }),
    ).toThrow("raised no host-fallback ask");
  });

  test("a rule-denied fallback row needs no ask, because the host never dispatches the tool", () => {
    expect(() =>
      assertBashFallbackAskIdentity(scenario("bash/T3/fallback_config_deny"), {
        resultText: "Permission denied: this session's permission rules deny bash",
        events: [],
      }),
    ).not.toThrow();
  });

  test("an allowed fallback row that raised no break-glass ask never reached the path", () => {
    expect(() =>
      assertBashFallbackAskIdentity(scenario("bash/T3/fallback_ask_allow"), {
        resultText: "[aft-bridge] Binary crashed (restarts: 0)",
        events: [],
      }),
    ).toThrow("raised no host-fallback ask");
  });

  test("an allowed fallback row that raised the ask took the declared path", () => {
    expect(() =>
      assertBashFallbackAskIdentity(scenario("bash/T3/fallback_ask_allow"), {
        resultText: "",
        events: [askedFor(`${HOST_FALLBACK_ASK_MARKER} (transport down) - host fallback`)],
      }),
    ).not.toThrow();
  });

  test("a loop row with neither mark took the declared path", () => {
    expect(() =>
      assertBashFallbackAskIdentity(scenario("bash/T3/loop_ask_allow"), {
        resultText: "loop permission fixture",
        events: [askedFor("printf 'loop permission fixture' > bash-loop.txt")],
      }),
    ).not.toThrow();
  });
});

describe("AFT task rows say whether AFT ran the command", () => {
  test("an allowed loop row without a task row means AFT ran nothing", () => {
    expect(() => assertBashAftExecutionIdentity(scenario("bash/T3/loop_ask_allow"), [])).toThrow(
      "left no task row",
    );
  });

  test("an allowed loop row with a task row ran through AFT", () => {
    expect(() =>
      assertBashAftExecutionIdentity(scenario("bash/T3/loop_ask_allow"), ["bash-1a2b3c4d"]),
    ).not.toThrow();
  });

  test("a denied row with a task row ran a command it was refused", () => {
    expect(() =>
      assertBashAftExecutionIdentity(scenario("bash/T3/loop_config_deny"), ["bash-1a2b3c4d"]),
    ).toThrow("still reached AFT");
  });

  test("an allowed fallback row with a task row kept a live transport", () => {
    expect(() =>
      assertBashAftExecutionIdentity(scenario("bash/T3/fallback_ask_allow"), ["bash-1a2b3c4d"]),
    ).toThrow("transport-dead window did not take");
  });

  test("denied rows without task rows ran nothing", () => {
    for (const id of ["bash/T3/loop_ask_deny", "bash/T3/fallback_config_deny"]) {
      expect(() => assertBashAftExecutionIdentity(scenario(id), [])).not.toThrow();
    }
  });
});
