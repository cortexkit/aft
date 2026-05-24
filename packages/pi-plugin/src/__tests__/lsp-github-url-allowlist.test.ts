/** Audit-3 v0.17 #5: same allowlist test as OpenCode plugin. */

import { afterEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  _assertAllowedDownloadUrlForTesting as assertAllowedDownloadUrl,
  _downloadFileForTesting as downloadFile,
} from "../lsp-github-install.js";

const tempRoots = new Set<string>();

afterEach(() => {
  for (const root of tempRoots) rmSync(root, { recursive: true, force: true });
  tempRoots.clear();
});

describe("downloadFile URL allowlist (audit-3 #5)", () => {
  test("accepts canonical github.com release-asset URL", () => {
    expect(() =>
      assertAllowedDownloadUrl(
        "https://github.com/clangd/clangd/releases/download/18.1.3/clangd-mac-18.1.3.zip",
      ),
    ).not.toThrow();
  });

  test("accepts objects.githubusercontent.com", () => {
    expect(() =>
      assertAllowedDownloadUrl(
        "https://objects.githubusercontent.com/github-production-release-asset-2e65be/123/abc",
      ),
    ).not.toThrow();
  });

  test("rejects an attacker-controlled host", () => {
    expect(() => assertAllowedDownloadUrl("https://evil.example/payload.zip")).toThrow(
      /not in the GitHub allowlist/,
    );
  });

  test("rejects http (downgrade attack)", () => {
    expect(() => assertAllowedDownloadUrl("http://github.com/x.zip")).toThrow(/must be https/);
  });

  test("rejects file:// URLs", () => {
    expect(() => assertAllowedDownloadUrl("file:///etc/passwd")).toThrow(/must be https/);
  });

  test("rejects subdomain confusion", () => {
    expect(() => assertAllowedDownloadUrl("https://github.com.evil.example/x")).toThrow(
      /not in the GitHub allowlist/,
    );
  });

  test("is case-insensitive on hostname", () => {
    expect(() => assertAllowedDownloadUrl("https://GITHUB.COM/x.zip")).not.toThrow();
  });

  test("rejects an allowed GitHub URL that redirects to an attacker host", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-gh-redirect-"));
    tempRoots.add(root);
    const dest = join(root, "payload.zip");
    const seen: string[] = [];
    const fetchImpl = (async (url: string | URL | Request) => {
      seen.push(String(url));
      return new Response(null, {
        status: 302,
        headers: { location: "https://evil.example/payload.zip" },
      });
    }) as typeof fetch;

    await expect(
      downloadFile(
        "https://github.com/owner/repo/releases/download/v1/payload.zip",
        dest,
        fetchImpl,
      ),
    ).rejects.toThrow(/not in the GitHub allowlist/);
    expect(seen).toEqual(["https://github.com/owner/repo/releases/download/v1/payload.zip"]);
    expect(existsSync(dest)).toBe(false);
  });
});
