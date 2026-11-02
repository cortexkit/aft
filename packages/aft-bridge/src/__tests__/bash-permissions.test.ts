import { expect, test } from "bun:test";
import {
  BASH_PERMISSION_DENIED,
  BashPermissionLoopError,
  withBashPermissionLoop,
} from "../bash-permissions.js";

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

test("shell command refusal message preserves the approved wording", () => {
  expect(BASH_PERMISSION_DENIED).toBe(
    "Permission denied for this shell command: {parts} ({host reason}). The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.",
  );
});

test("permission loop reports a round-0 refusal of both shell command parts without retrying", async () => {
  const asks = [
    { kind: "bash", patterns: ["mkdir -p x"] },
    { kind: "bash", patterns: ["rm -rf x"] },
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
  expect(error.message).toBe(
    "Permission denied for this shell command: mkdir -p x, rm -rf x (Permission denied: shell). The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.",
  );
  expect(error.message).not.toContain("not settled");
  expect(error.cause).toBe(cause);
  expect(requests).toBe(1);
  expect(approvals).toBe(1);
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
    "Permission denied for this shell command: rm -rf x (Permission denied: shell). The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.",
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
    "Permission denied for this shell command: mkdir -p x && rm -rf x @ /workspace (Permission denied: host). The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.",
  );
});

test.each([
  new Error(),
  undefined,
  null,
])("permission loop supplies a fallback when the host throws without a message (%p)", async (cause) => {
  const error = await withBashPermissionLoop({
    request: async () => required,
    approve: async () => {
      throw cause;
    },
  }).catch((error) => error);
  expect(error).toBeInstanceOf(BashPermissionLoopError);
  expect(error.message).toBe(
    "Permission denied for this shell command: /outside/worker/* (no host reason provided). The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.",
  );
  expect(error.cause).toBe(cause);
});
