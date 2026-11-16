import { expect, test } from "bun:test";
import {
  BASH_PERMISSION_DENIED,
  BASH_PERMISSION_DENIED_KNOWN,
  BashPermissionLoopError,
  withBashPermissionLoop,
} from "../bash-permissions.js";
import {
  SHELL_REFUSAL_ALL_DENIED,
  SHELL_REFUSAL_KNOWN,
  SHELL_REFUSAL_TWO_DENIED,
  SHELL_REFUSAL_UNKNOWN,
} from "./fixtures/bash-permission-refusals.js";

const ask = {
  kind: "external_directory",
  patterns: ["/outside/worker/*"],
  always: ["/outside/worker/*"],
};
const required = { success: false, code: "permission_required", asks: [ask] };

test("permission loop stops on already granted asks", async () => {
  const calls: string[][] = [];
  let approvals = 0;
  await expect(
    withBashPermissionLoop({
      request: async (grants) => {
        calls.push([...grants]);
        return required;
      },
      approve: async () => {
        approvals++;
      },
    }),
  ).rejects.toThrow(
    "bash permission not settled after 1 rounds: still asking for external_directory /outside/worker/* (operation: bash; granted already: yes)",
  );
  expect(calls).toEqual([[], ["/outside/worker/*"]]);
  expect(approvals).toBe(1);
});

test("permission loop names the final outstanding ask at the round limit", async () => {
  let calls = 0;
  let approvals = 0;
  await expect(
    withBashPermissionLoop({
      request: async () => ({
        ...required,
        asks: [{ ...ask, patterns: [`/outside/${calls}`], always: [`/outside/${calls++}`] }],
      }),
      approve: async () => {
        approvals++;
      },
      operation: "powershell",
    }),
  ).rejects.toThrow(
    "bash permission not settled after 8 rounds: still asking for external_directory /outside/8 (operation: powershell; granted already: no)",
  );
  expect(calls).toBe(9);
  expect(approvals).toBe(8);
});

test("permission loop accepts success on the last retry", async () => {
  let calls = 0;
  expect(
    await withBashPermissionLoop({
      request: async () =>
        calls++ === 8
          ? { success: true }
          : { ...required, asks: [{ kind: "bash", patterns: [`command ${calls}`] }] },
      approve: async () => {},
    }),
  ).toEqual({ success: true });
});

test("permission loop reports missing asks", async () => {
  await expect(
    withBashPermissionLoop({
      request: async () => ({ ...required, asks: [] }),
      approve: async () => {
        throw new Error("must not prompt");
      },
    }),
  ).rejects.toThrow(
    "bash permission not settled after 0 rounds: still asking for unknown <no asks returned> (operation: bash; granted already: no)",
  );
});

test("permission loop refusal templates preserve the approved wording", () => {
  expect(BASH_PERMISSION_DENIED).toBe(
    "Permission denied for this shell command: one or more of these parts was denied: {parts} ({host reason}). The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.",
  );
  expect(BASH_PERMISSION_DENIED_KNOWN).toBe(
    "Permission denied for this shell command: the host denied {parts} ({host reason}). These parts were not denied: {not denied parts}. The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.",
  );
});

test("permission loop reports a round-0 refusal of both shell command parts without retrying", async () => {
  const asks = [
    { kind: "bash", patterns: ["mkdir -p x"] },
    { kind: "bash", patterns: ["git ls-tree HEAD src"] },
  ];
  const cause = new Error("Permission denied: shell");
  let requests = 0;
  let approvals = 0;
  const error = await withBashPermissionLoop({
    request: async (grants) => {
      requests++;
      expect(grants).toEqual([]);
      return { ...required, asks };
    },
    approve: async (pendingAsks) => {
      approvals++;
      expect(pendingAsks).toEqual(asks);
      throw cause;
    },
  }).catch((error) => error);
  expect(error).toBeInstanceOf(BashPermissionLoopError);
  expect(error.message).toBe(SHELL_REFUSAL_UNKNOWN);
  expect(error.message).not.toContain("not settled");
  expect(error.cause).toBe(cause);
  expect(requests).toBe(1);
  expect(approvals).toBe(1);
});

test.each([
  [
    "one denied part",
    ["mkdir -p x", "git ls-tree HEAD src"],
    ["git ls-tree *"],
    SHELL_REFUSAL_KNOWN,
  ],
  [
    "two denied parts",
    ["mkdir -p x", "git ls-tree HEAD src", "git show HEAD"],
    ["git ls-tree *", "git show *"],
    SHELL_REFUSAL_TWO_DENIED,
  ],
  [
    "one denied part with two others not denied",
    ["mkdir -p x", "git ls-tree HEAD src", "git show HEAD"],
    ["git ls-tree *"],
    SHELL_REFUSAL_KNOWN.replace("`mkdir -p x`", "`mkdir -p x`, `git show HEAD`"),
  ],
  ["all parts denied", ["mkdir -p x", "git ls-tree HEAD src"], ["*"], SHELL_REFUSAL_ALL_DENIED],
  [
    "no matching deny rule",
    ["mkdir -p x", "git ls-tree HEAD src"],
    ["git diff *"],
    SHELL_REFUSAL_UNKNOWN,
  ],
])("permission loop attributes %s without extra approvals", async (_label, parts, patterns, expected) => {
  const cause = Object.assign(new Error("Permission denied: shell"), {
    _tag: "Permission.BlockedError",
    permission: "shell",
    resources: parts,
    rules: patterns.map((resource) => ({ action: "shell", resource, effect: "deny" })),
  });
  let requests = 0;
  let approvals = 0;
  const error = await withBashPermissionLoop({
    request: async () => {
      requests++;
      return { ...required, asks: [{ kind: "bash", patterns: parts }] };
    },
    approve: async (asks) => {
      approvals++;
      expect(asks).toEqual([{ kind: "bash", patterns: parts }]);
      throw cause;
    },
  }).catch((error) => error);
  expect(error.message).toBe(expected);
  expect(error.cause).toBe(cause);
  expect(requests).toBe(1);
  expect(approvals).toBe(1);
});

test.each([
  ["allow", "shell", "git ls-tree *"],
  ["ask", "shell", "git ls-tree *"],
  ["deny", "read", "git ls-tree *"],
])("permission loop follows the last matching rule (%s for %s)", async (effect, action, resource) => {
  const parts = ["mkdir -p x", "git ls-tree HEAD src"];
  const rules = [
    { action: "shell", resource: "git ls-tree *", effect: "deny" },
    { action, resource, effect },
  ];
  // A later matching allow/ask overrides the deny, but another action does not.
  const expected = action === "shell" ? SHELL_REFUSAL_UNKNOWN : SHELL_REFUSAL_KNOWN;
  const cause = Object.assign(new Error("Permission denied: shell"), {
    _tag: "Permission.BlockedError",
    permission: "shell",
    resources: parts,
    rules,
  });
  const error = await withBashPermissionLoop({
    request: async () => ({ ...required, asks: [{ kind: "bash", patterns: parts }] }),
    approve: async () => {
      throw cause;
    },
  }).catch((error) => error);
  expect(error.message).toBe(expected);
});

test.each([
  { rules: undefined },
  { rules: [{ action: "shell", resource: "*", effect: "unexpected" }] },
  { resources: ["another command"] },
  { permission: "read" },
])("permission loop falls back when host rule evidence is unavailable (%p)", async (override) => {
  const parts = ["mkdir -p x", "git ls-tree HEAD src"];
  const cause = Object.assign(new Error("Permission denied: shell"), {
    _tag: "Permission.BlockedError",
    permission: "shell",
    resources: parts,
    rules: [{ action: "shell", resource: "*", effect: "deny" }],
    ...override,
  });
  const error = await withBashPermissionLoop({
    request: async () => ({ ...required, asks: [{ kind: "bash", patterns: parts }] }),
    approve: async () => {
      throw cause;
    },
  }).catch((error) => error);
  expect(error.message).toBe(SHELL_REFUSAL_UNKNOWN);
});

// OpenCode @opencode/core 2.1.0's default rules use *.env, *.env.* and
// *.env.example (dist/chunks/shell-pdf17x7p.js:105). The remaining cases pin
// the matcher branches in dist/chunks/shell-y1fvbwt7.js:11-16.
test.each([
  ["secret.env", "*.env", true],
  ["secret.env.local", "*.env.*", true],
  ["secret.env.example", "*.env.example", true],
  ["secret.env.local", "*.env", false],
  ["git", "git *", true],
  ["git show HEAD", "git *", true],
  ["gitx", "git *", false],
  ["git ls-tree", "git ls-?ree", true],
  ["git ls-xxree", "git ls-?ree", false],
  ["git show\nHEAD", "git *", true],
  ["git show\nHEAD", "git show?HEAD", true],
  ["C:\\work\\cmd", "C:/work/*", true],
  ["C:/work/cmd", "C:\\work\\*", true],
  ["printf .+^${}()|[]$&", "printf .+^${}()|[]$&", true],
  ["echo axb", "echo a.b", false],
  ["git show HEAD extra", "git show HEAD", false],
  ["Git show HEAD", "git *", process.platform === "win32"],
])("permission loop matches OpenCode wildcard %p against %p (%p)", async (part, pattern, matches) => {
  const parts = ["mkdir -p x", part];
  const cause = Object.assign(new Error("Permission denied: shell"), {
    _tag: "Permission.BlockedError",
    permission: "shell",
    resources: parts,
    rules: [{ action: "sh?ll", resource: pattern, effect: "deny" }],
  });
  const error = await withBashPermissionLoop({
    request: async () => ({ ...required, asks: [{ kind: "bash", patterns: parts }] }),
    approve: async () => {
      throw cause;
    },
  }).catch((error) => error);
  expect(error.message).toBe(
    (matches ? SHELL_REFUSAL_KNOWN : SHELL_REFUSAL_UNKNOWN).replace(
      "git ls-tree HEAD src",
      () => part,
    ),
  );
});

test("permission loop reports a later refusal with only the newly asked parts", async () => {
  const laterAsk = { kind: "bash", patterns: ["rm -rf x"] };
  const cause = new Error("Permission denied: shell");
  const requests: string[][] = [];
  let approvals = 0;
  const error = await withBashPermissionLoop({
    request: async (grants) => {
      requests.push([...grants]);
      return requests.length === 1 ? required : { ...required, asks: [ask, laterAsk] };
    },
    approve: async (pendingAsks) => {
      if (++approvals === 1) {
        expect(pendingAsks).toEqual([ask]);
        return;
      }
      expect(pendingAsks).toEqual([laterAsk]);
      throw cause;
    },
  }).catch((error) => error);
  expect(error).toBeInstanceOf(BashPermissionLoopError);
  expect(error.message).toBe(
    "Permission denied for this shell command: one or more of these parts was denied: `rm -rf x` (Permission denied: shell). The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.",
  );
  expect(error.cause).toBe(cause);
  expect(requests).toEqual([[], ["/outside/worker/*"]]);
  expect(approvals).toBe(2);
});

test("permission loop describes refused escalation with its command and working directory", async () => {
  await expect(
    withBashPermissionLoop({
      request: async () => ({
        ...required,
        asks: [
          {
            kind: "escalation",
            command: "mkdir -p x && rm -rf x",
            cwd: "/workspace",
            grant_id: "host-escalation",
          },
        ],
      }),
      approve: async () => {
        throw new Error("Permission denied: host");
      },
    }),
  ).rejects.toThrow(
    "Permission denied for this shell command: one or more of these parts was denied: `mkdir -p x && rm -rf x @ /workspace` (Permission denied: host). The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.",
  );
});

test.each([
  new Error(),
  undefined,
  null,
])("permission loop omits the host reason when the host throws without a message (%p)", async (cause) => {
  const error = await withBashPermissionLoop({
    request: async () => required,
    approve: async () => {
      throw cause;
    },
  }).catch((error) => error);
  expect(error).toBeInstanceOf(BashPermissionLoopError);
  expect(error.message).toBe(
    "Permission denied for this shell command: one or more of these parts was denied: `/outside/worker/*`. The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.",
  );
  expect(error.cause).toBe(cause);
});
