import { describe, expect, test } from "bun:test";

import { withPathPrepended } from "../path-env.js";

function windowsPathKeys(env: NodeJS.ProcessEnv): string[] {
  return Object.keys(env).filter((key) => key.toLowerCase() === "path");
}

function expectSingleWindowsPath(env: NodeJS.ProcessEnv, key: string, value: string): void {
  expect(windowsPathKeys(env)).toEqual([key]);
  expect(env[key]).toBe(value);
}

describe("withPathPrepended", () => {
  test("keeps inherited Windows Path spelling and prepends the ONNX directory", () => {
    const input = { Path: "C:\\Windows\\System32;C:\\Git\\cmd", HOME: "C:\\Users\\me" };

    const output = withPathPrepended(input, "C:\\onnxruntime", "win32");

    expectSingleWindowsPath(output, "Path", "C:\\onnxruntime;C:\\Windows\\System32;C:\\Git\\cmd");
    expect(output.HOME).toBe(input.HOME);
    expect(input).toEqual({
      Path: "C:\\Windows\\System32;C:\\Git\\cmd",
      HOME: "C:\\Users\\me",
    });
  });

  test("keeps inherited Windows PATH spelling", () => {
    const output = withPathPrepended({ PATH: "C:\\Windows\\System32" }, "C:\\onnxruntime", "win32");

    expectSingleWindowsPath(output, "PATH", "C:\\onnxruntime;C:\\Windows\\System32");
  });

  test("collapses duplicate Windows path variants to one inherited key", () => {
    const output = withPathPrepended(
      {
        Path: "C:\\Windows\\System32;C:\\Git\\cmd",
        PATH: "C:\\stale",
        path: "C:\\also-stale",
      },
      "C:\\onnxruntime",
      "win32",
    );

    expectSingleWindowsPath(output, "Path", "C:\\onnxruntime;C:\\Windows\\System32;C:\\Git\\cmd");
  });

  test("uses PATH when Windows has no inherited path key", () => {
    const output = withPathPrepended({ TEMP: "C:\\Temp" }, "C:\\onnxruntime", "win32");

    expectSingleWindowsPath(output, "PATH", "C:\\onnxruntime");
  });

  test("normalizes Windows path variants without an ONNX directory", () => {
    const output = withPathPrepended(
      { Path: "C:\\Windows\\System32", PATH: "C:\\stale" },
      undefined,
      "win32",
    );

    expectSingleWindowsPath(output, "Path", "C:\\Windows\\System32");
  });

  test("adds no path key when Windows has neither a path nor a directory", () => {
    const output = withPathPrepended({ TEMP: "C:\\Temp" }, null, "win32");

    expect(windowsPathKeys(output)).toEqual([]);
    expect(output).toEqual({ TEMP: "C:\\Temp" });
  });

  test("on non-Windows prepends only exact PATH and leaves case variants untouched", () => {
    const output = withPathPrepended(
      { PATH: "/usr/bin", Path: "/case-sensitive/Path", path: "/case-sensitive/path" },
      "/opt/node/bin",
      "linux",
    );

    expect(output).toEqual({
      PATH: "/opt/node/bin:/usr/bin",
      Path: "/case-sensitive/Path",
      path: "/case-sensitive/path",
    });
  });

  test("on non-Windows adds PATH without treating Path as inherited PATH", () => {
    const output = withPathPrepended(
      { Path: "/not-an-executable-path" },
      "/opt/node/bin",
      "darwin",
    );

    expect(output).toEqual({
      PATH: "/opt/node/bin",
      Path: "/not-an-executable-path",
    });
  });
});
