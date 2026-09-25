import { describe, expect, test } from "bun:test";
import {
  loadFeaturePlan,
  type NativeResult,
  type PlanFeature,
  type SetupPlan,
} from "../lib/feature-plan.js";
import {
  buildAnswers,
  explanationLines,
  featureMode,
  githubReadDisplay,
  groupRows,
  initialGithubChoice,
  initialSelections,
  renderFeatureStatus,
  runFeatureSetup,
  runFeatureWizard,
  setGithubRead,
  setGithubWrite,
  type WizardIO,
} from "../setup/feature-wizard.js";

function row(id: string, overrides: Partial<PlanFeature> = {}): PlanFeature {
  const kind = id.startsWith("indexes.")
    ? "index"
    : id.startsWith("github.")
      ? "capability"
      : "tool";
  return {
    id,
    kind,
    group: kind === "index" ? "Indexes" : kind === "capability" ? "GitHub" : "Search/navigation",
    order: 0,
    label: id,
    description: `${id} description`,
    binding: {
      path: kind === "tool" ? "disabled_tools" : id,
      tool_name: kind === "tool" ? id : null,
    },
    default: kind !== "capability",
    configured: kind !== "capability",
    source: "default",
    proposed: kind !== "capability",
    effective: kind === "capability" ? "off" : "ready",
    reason: "default",
    available: true,
    unavailable_reason: null,
    cost_note: null,
    prerequisites: [],
    ...overrides,
  };
}

function fixturePlan(overrides: Record<string, Partial<PlanFeature>> = {}): SetupPlan {
  const ids = [
    "grep",
    "glob",
    "aft_move",
    "aft_delete",
    "indexes.semantic",
    "github.read",
    "github.write",
  ];
  return {
    plan_version: 1,
    features: ids.map((id, index) => row(id, { order: index + 1, ...overrides[id] })),
  };
}

describe("GitHub read/write lock", () => {
  test("checking write locks read on and unchecking restores the independent choice", () => {
    let choice = initialGithubChoice(fixturePlan());
    expect(githubReadDisplay(choice)).toEqual({ checked: false, locked: false });
    choice = setGithubWrite(choice, true);
    expect(githubReadDisplay(choice)).toEqual({ checked: true, locked: true });
    expect(setGithubRead(choice, false)).toBe(choice);
    choice = setGithubWrite(choice, false);
    expect(githubReadDisplay(choice)).toEqual({ checked: false, locked: false });
  });

  test("the independent read choice comes from configured, not the implied proposal", () => {
    const plan = fixturePlan({
      "github.read": { configured: false, proposed: true, effective: "ready" },
      "github.write": { configured: true, proposed: true, source: "config" },
    });
    const choice = initialGithubChoice(plan);
    expect(choice).toEqual({ read: false, write: true, readEdited: false });
    expect(githubReadDisplay(setGithubWrite(choice, false)).checked).toBe(false);
  });

  test("a write-only save leaves read out of the answers", () => {
    const plan = fixturePlan();
    const choice = setGithubWrite(initialGithubChoice(plan), true);
    const answers = buildAnswers(plan, initialSelections(plan), choice);
    expect(answers.selections["github.write"]).toBe(true);
    expect("github.read" in answers.selections).toBe(false);
  });

  test("an edited read choice is kept even when write is then checked", () => {
    const plan = fixturePlan();
    let choice = setGithubRead(initialGithubChoice(plan), true);
    choice = setGithubWrite(choice, true);
    expect(buildAnswers(plan, {}, choice).selections["github.read"]).toBe(true);
  });
});

describe("feature wizard rendering", () => {
  test("rows are grouped by the plan with separate grep and glob rows; GitHub is not a checkbox", () => {
    const groups = groupRows(fixturePlan());
    expect([...groups.keys()]).toEqual(["Search/navigation", "Indexes"]);
    expect(groups.get("Search/navigation")?.map((feature) => feature.id)).toEqual([
      "grep",
      "glob",
      "aft_move",
      "aft_delete",
    ]);
  });

  test("explanations come from the plan, including costs and platform adjustments", () => {
    const plan = fixturePlan({
      "indexes.semantic": {
        cost_note: "may download an ONNX runtime and use CPU",
        proposed: false,
        unavailable_reason: "semantic_backend_unsupported_platform",
        effective: "unavailable",
      },
      aft_move: { description: "backed up for undo" },
      aft_delete: { description: "refuses symlinks" },
    });
    const lines = explanationLines(plan).join("\n");
    expect(lines).toContain("backed up for undo");
    expect(lines).toContain("refuses symlinks");
    expect(lines).toContain("may download an ONNX runtime and use CPU");
    expect(lines).toContain("semantic_backend_unsupported_platform");
  });

  test("an untouched save sends the proposed values for every row", async () => {
    const plan = fixturePlan({
      aft_move: { configured: false, proposed: false, effective: "off" },
      aft_delete: { configured: false, proposed: false, effective: "off" },
      "indexes.semantic": { proposed: false },
    });
    const io: WizardIO = {
      selectRows: async (_message, _options, initial) => initial,
      confirm: async (_message, initial) => initial,
      info: () => {},
      note: () => {},
    };
    const answers = await runFeatureWizard(plan, io);
    expect(answers).toEqual({
      plan_version: 1,
      selections: {
        grep: true,
        glob: true,
        aft_move: false,
        aft_delete: false,
        "indexes.semantic": false,
        "github.write": false,
        "github.read": false,
      },
    });
  });

  test("GitHub read is asked first; write is offered only once read is on", async () => {
    const plan = fixturePlan();
    const asked: string[] = [];
    const io: WizardIO = {
      selectRows: async (_message, _options, initial) => initial,
      confirm: async (message) => {
        asked.push(message);
        return true;
      },
      info: () => {},
      note: () => {},
    };
    const answers = await runFeatureWizard(plan, io, () => "ready");
    expect(asked).toHaveLength(2);
    expect(asked[0]).toContain("read GitHub issues and pull requests");
    expect(asked[0]).not.toContain("issue://");
    expect(asked[1]).toBe("github.write: github.write description");
    // Write implies read, so an unedited implied read stays out of the file.
    expect(answers.selections["github.write"]).toBe(true);
    expect("github.read" in answers.selections).toBe(false);
  });

  test("declining GitHub read skips the write question and saves both off", async () => {
    const plan = fixturePlan({ "github.write": { proposed: true, configured: true } });
    const asked: string[] = [];
    const io: WizardIO = {
      selectRows: async (_message, _options, initial) => initial,
      confirm: async (message) => {
        asked.push(message);
        return false;
      },
      info: () => {},
      note: () => {},
    };
    const answers = await runFeatureWizard(plan, io, () => "ready");
    expect(asked).toHaveLength(1);
    expect(answers.selections["github.write"]).toBe(false);
    expect(answers.selections["github.read"]).toBe(false);
  });

  test("enabling GitHub read warns once when gh is missing or signed out, and never blocks", async () => {
    for (const [status, needle] of [
      ["missing", "not on PATH"],
      ["signed_out", "gh auth login"],
    ] as const) {
      const warned: string[] = [];
      let checks = 0;
      const io: WizardIO = {
        selectRows: async (_message, _options, initial) => initial,
        confirm: async (message) => message.includes("read GitHub"),
        info: () => {},
        warn: (message) => warned.push(message),
        note: () => {},
      };
      const answers = await runFeatureWizard(fixturePlan(), io, () => {
        checks += 1;
        return status;
      });
      expect(checks).toBe(1);
      expect(warned).toHaveLength(1);
      expect(warned[0]).toContain(needle);
      expect(answers.selections["github.read"]).toBe(true);
    }
  });

  test("no GitHub CLI check runs when GitHub read is declined", async () => {
    let checks = 0;
    const io: WizardIO = {
      selectRows: async (_message, _options, initial) => initial,
      confirm: async () => false,
      info: () => {},
      note: () => {},
    };
    await runFeatureWizard(fixturePlan(), io, () => {
      checks += 1;
      return "missing";
    });
    expect(checks).toBe(0);
  });

  test("doctor lines carry configured/effective/source, the reason and the cause", () => {
    const lines = renderFeatureStatus(
      fixturePlan({
        "indexes.semantic": {
          effective: "unavailable",
          available: false,
          unavailable_reason: "runtime_not_observed",
        },
      }),
    );
    expect(lines).toContain(
      "indexes.semantic: unavailable — configured on (default); reason: default; unavailable: runtime_not_observed",
    );
    expect(lines).toContain("github.read: off — configured off (default); reason: default");
  });
});

describe("feature setup modes", () => {
  function recorder(result: Partial<NativeResult> = {}) {
    const calls: { args: string[]; input?: string }[] = [];
    const run = (args: string[], input?: string): NativeResult => {
      calls.push({ args, input });
      return { ok: true, stdout: "", stderr: "", status: 0, ...result };
    };
    return { calls, run };
  }

  test("modes are chosen from argv", () => {
    expect(featureMode(["--plan"])).toBe("plan");
    expect(featureMode(["--yes"])).toBe("yes");
    expect(featureMode(["--answers", "a.json"])).toBe("answers");
    expect(featureMode([])).toBe("interactive");
  });

  test("--yes and --answers together fail without calling the binary", async () => {
    const { calls, run } = recorder();
    let err = "";
    const code = await runFeatureSetup(["--yes", "--answers", "a.json"], {
      run,
      stderr: (text) => {
        err += text;
      },
    });
    expect(code).toBe(2);
    expect(err).toContain("mutually exclusive");
    expect(calls).toEqual([]);
  });

  test("--plan forwards only an explicit harness and prints the binary's stdout", async () => {
    const { calls, run } = recorder({ stdout: '{"plan_version":1,"features":[]}\n' });
    let out = "";
    await runFeatureSetup(["--plan"], { run, stdout: (text) => (out += text) });
    await runFeatureSetup(["--plan", "--harness", "omp"], { run, stdout: () => {} });
    expect(calls.map((call) => call.args)).toEqual([
      ["setup", "--plan"],
      ["setup", "--plan", "--harness", "omp"],
    ]);
    expect(out).toBe('{"plan_version":1,"features":[]}\n');
  });

  test("the interactive save writes answers once and does not repeat load warnings", async () => {
    const plan = fixturePlan();
    const calls: string[][] = [];
    const inputs: (string | undefined)[] = [];
    const run = (args: string[], input?: string): NativeResult => {
      calls.push(args);
      inputs.push(input);
      return args.includes("--plan")
        ? { ok: true, stdout: JSON.stringify(plan), stderr: "", status: 0 }
        : { ok: true, stdout: "{}", stderr: "", status: 0 };
    };
    const io: WizardIO = {
      selectRows: async (_m, _o, initial) => initial,
      confirm: async (_m, initial) => initial,
      info: () => {},
      note: () => {},
    };
    expect(await runFeatureSetup([], { run, io, interactive: true })).toBe(0);
    expect(calls).toEqual([
      ["setup", "--plan"],
      ["setup", "--answers", "-", "--no-load-warnings"],
    ]);
    expect(JSON.parse(inputs[1] ?? "{}").plan_version).toBe(1);
  });

  test("an unknown plan version is refused", () => {
    const loaded = loadFeaturePlan(null, () => ({
      ok: true,
      stdout: JSON.stringify({ plan_version: 2, features: [] }),
      stderr: "",
      status: 0,
    }));
    expect(loaded).toEqual({
      ok: false,
      error: "unsupported_setup_plan_version",
      configRejected: false,
    });
  });
});
