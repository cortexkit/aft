import { expect, test } from "bun:test";
import { formatUnknownPtyExit } from "../bash-format.js";

test("PTY lost exit header uses approved words without changing other unknown fates or kills", () => {
  expect(
    formatUnknownPtyExit("bash-lost", {
      status: "fate_unknown",
      status_reason: "AFT could not read how it ended",
    }),
  ).toBe("Task bash-lost: exit status unknown (AFT could not read how it ended)");
  expect(
    formatUnknownPtyExit("bash-killed", {
      status: "killed",
      status_reason: "AFT could not read how it ended",
    }),
  ).toBeUndefined();
  expect(
    formatUnknownPtyExit("bash-remote", {
      status: "fate_unknown",
      status_reason: "remote connection lost",
    }),
  ).toBeUndefined();
});
