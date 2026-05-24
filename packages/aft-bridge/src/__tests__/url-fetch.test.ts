/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fetchUrlToTempFile } from "../url-fetch.js";

let storageDir: string;

beforeEach(() => {
  storageDir = mkdtempSync(join(tmpdir(), "aft-url-fetch-"));
});

afterEach(() => {
  rmSync(storageDir, { recursive: true, force: true });
});

describe("fetchUrlToTempFile", () => {
  test("cache hits still enforce the current private-host policy", async () => {
    const privateUrl = "http://127.0.0.1/x";
    const fetchImpl = async () =>
      new Response("# cached private content\n", {
        headers: { "content-type": "text/markdown" },
      });

    await fetchUrlToTempFile(privateUrl, storageDir, {
      allowPrivate: true,
      fetchImpl,
    });

    await expect(
      fetchUrlToTempFile(privateUrl, storageDir, {
        allowPrivate: false,
        fetchImpl,
      }),
    ).rejects.toThrow(/Blocked private URL host/);
  });
});
