import { expect, test } from "bun:test";
import { malformedSecurityConfigErrors } from "../feature-config";

test("project_detection is a security section at base and active harness", () => {
  expect(
    malformedSecurityConfigErrors({}, "opencode", [{ path: ["project_detection", "mode"] }]),
  ).toEqual(["invalid_security_config:project_detection"]);
  expect(
    malformedSecurityConfigErrors(
      { harnesses: { opencode: { project_detection: { mode: "bad" } } } },
      "opencode",
      [{ path: ["harnesses", "opencode", "project_detection", "mode"] }],
    ),
  ).toEqual(["invalid_security_config:harnesses.opencode.project_detection"]);
});
