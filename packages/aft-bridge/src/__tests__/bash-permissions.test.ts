import { expect, test } from "bun:test";
import { withBashPermissionLoop } from "../bash-permissions.js";

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
  ).rejects.toThrow("external_directory /outside/worker/* (operation: bash; granted already: yes)");
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

test("permission loop reports missing asks and denied approvals", async () => {
  await expect(
    withBashPermissionLoop({
      request: async () => ({ ...required, asks: [] }),
      approve: async () => {
        throw new Error("must not prompt");
      },
    }),
  ).rejects.toThrow("unknown <no asks returned> (operation: bash; granted already: no)");
  await expect(
    withBashPermissionLoop({
      request: async () => required,
      approve: async () => {
        throw new Error("approval denied");
      },
    }),
  ).rejects.toThrow(
    "external_directory /outside/worker/* (operation: bash; granted already: no): approval denied",
  );
});
