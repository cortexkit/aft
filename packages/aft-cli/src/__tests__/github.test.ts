/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { getOpenBrowserCommand } from "../lib/github.js";

describe("openBrowser command selection", () => {
  test("uses explorer.exe on Windows so issue URLs with & are not shell-split", () => {
    const url = "https://github.com/cortexkit/aft/issues/new?title=x&body=y";

    expect(getOpenBrowserCommand(url, "win32")).toEqual(["explorer.exe", [url]]);
  });
});
