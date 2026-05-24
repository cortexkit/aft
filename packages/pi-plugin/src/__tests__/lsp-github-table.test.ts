/**
 * Asset-template correctness tests for `lsp-github-table.ts`.
 *
 * These guard against future regressions where a refactor accidentally
 * inverts arch/platform mapping or breaks the OpenCode parity. Each
 * spec asserts the exact asset name OpenCode would also produce.
 */

import { describe, expect, test } from "bun:test";
import { detectHostPlatform, findGithubServerById, GITHUB_LSP_TABLE } from "../lsp-github-table.js";

describe("findGithubServerById", () => {
  test("returns spec by id", () => {
    const clangd = findGithubServerById("clangd");
    expect(clangd?.id).toBe("clangd");
    expect(clangd?.githubRepo).toBe("clangd/clangd");
    expect(clangd?.binary).toBe("clangd");
  });

  test("returns undefined for unknown id", () => {
    expect(findGithubServerById("nonexistent")).toBeUndefined();
  });
});

describe("clangd asset template", () => {
  const clangd = findGithubServerById("clangd");
  if (!clangd) throw new Error("clangd missing");

  test("mac/arm64", () => {
    const a = clangd.resolveAsset("darwin", "arm64", "21.1.0");
    expect(a).toEqual({ name: "clangd-mac-21.1.0.zip", archive: "zip" });
  });
  test("mac/x64", () => {
    const a = clangd.resolveAsset("darwin", "x64", "21.1.0");
    expect(a).toEqual({ name: "clangd-mac-21.1.0.zip", archive: "zip" });
  });
  test("linux", () => {
    const a = clangd.resolveAsset("linux", "x64", "21.1.0");
    expect(a).toEqual({ name: "clangd-linux-21.1.0.zip", archive: "zip" });
  });
  test("windows", () => {
    const a = clangd.resolveAsset("win32", "x64", "21.1.0");
    expect(a).toEqual({ name: "clangd-windows-21.1.0.zip", archive: "zip" });
  });
  test("inner binary path embeds version", () => {
    expect(clangd.binaryPathInArchive("linux", "x64", "21.1.0")).toBe("clangd_21.1.0/bin/clangd");
    expect(clangd.binaryPathInArchive("win32", "x64", "21.1.0")).toBe(
      "clangd_21.1.0/bin/clangd.exe",
    );
  });
});

describe("lua-language-server asset template", () => {
  const lua = findGithubServerById("lua-ls");
  if (!lua) throw new Error("lua-ls missing");

  test("mac arm64 = darwin-arm64.tar.gz", () => {
    const a = lua.resolveAsset("darwin", "arm64", "3.10.5");
    expect(a).toEqual({
      name: "lua-language-server-3.10.5-darwin-arm64.tar.gz",
      archive: "tar.gz",
    });
  });
  test("linux x64 = linux-x64.tar.gz", () => {
    const a = lua.resolveAsset("linux", "x64", "3.10.5");
    expect(a).toEqual({
      name: "lua-language-server-3.10.5-linux-x64.tar.gz",
      archive: "tar.gz",
    });
  });
  test("windows x64 = win32-x64.zip", () => {
    const a = lua.resolveAsset("win32", "x64", "3.10.5");
    expect(a).toEqual({
      name: "lua-language-server-3.10.5-win32-x64.zip",
      archive: "zip",
    });
  });
  test("inner binary path uses bin/", () => {
    expect(lua.binaryPathInArchive("linux", "x64", "3.10.5")).toBe("bin/lua-language-server");
    expect(lua.binaryPathInArchive("win32", "x64", "3.10.5")).toBe("bin/lua-language-server.exe");
  });
});

describe("zls asset template", () => {
  const zls = findGithubServerById("zls");
  if (!zls) throw new Error("zls missing");

  test("mac aarch64 maps to macos", () => {
    expect(zls.resolveAsset("darwin", "arm64", "0.13.0")).toEqual({
      name: "zls-aarch64-macos.tar.xz",
      archive: "tar.xz",
    });
  });
  test("linux x86_64", () => {
    expect(zls.resolveAsset("linux", "x64", "0.13.0")).toEqual({
      name: "zls-x86_64-linux.tar.xz",
      archive: "tar.xz",
    });
  });
  test("windows uses zip", () => {
    expect(zls.resolveAsset("win32", "x64", "0.13.0")).toEqual({
      name: "zls-x86_64-windows.zip",
      archive: "zip",
    });
  });
});

describe("tinymist asset template", () => {
  const tinymist = findGithubServerById("tinymist");
  if (!tinymist) throw new Error("tinymist missing");

  test("mac uses apple-darwin triple", () => {
    expect(tinymist.resolveAsset("darwin", "arm64", "0.13.0")).toEqual({
      name: "tinymist-aarch64-apple-darwin.tar.gz",
      archive: "tar.gz",
    });
  });
  test("linux uses unknown-linux-gnu triple", () => {
    expect(tinymist.resolveAsset("linux", "x64", "0.13.0")).toEqual({
      name: "tinymist-x86_64-unknown-linux-gnu.tar.gz",
      archive: "tar.gz",
    });
  });
  test("windows uses pc-windows-msvc triple + zip", () => {
    expect(tinymist.resolveAsset("win32", "x64", "0.13.0")).toEqual({
      name: "tinymist-x86_64-pc-windows-msvc.zip",
      archive: "zip",
    });
  });
});

describe("texlab asset template", () => {
  const texlab = findGithubServerById("texlab");
  if (!texlab) throw new Error("texlab missing");

  test("mac arm64 → aarch64-macos", () => {
    expect(texlab.resolveAsset("darwin", "arm64", "5.21.0")).toEqual({
      name: "texlab-aarch64-macos.tar.gz",
      archive: "tar.gz",
    });
  });
  test("linux x64 → x86_64-linux.tar.gz", () => {
    expect(texlab.resolveAsset("linux", "x64", "5.21.0")).toEqual({
      name: "texlab-x86_64-linux.tar.gz",
      archive: "tar.gz",
    });
  });
  test("windows → x86_64-windows.zip", () => {
    expect(texlab.resolveAsset("win32", "x64", "5.21.0")).toEqual({
      name: "texlab-x86_64-windows.zip",
      archive: "zip",
    });
  });
});

describe("server table contents", () => {
  test("includes exactly the v0.17.0 set", () => {
    const ids = GITHUB_LSP_TABLE.map((s) => s.id).sort();
    expect(ids).toEqual(["clangd", "lua-ls", "texlab", "tinymist", "zls"]);
  });

  test("every server has a non-empty github repo + binary", () => {
    for (const spec of GITHUB_LSP_TABLE) {
      expect(spec.githubRepo).toMatch(/^[\w.-]+\/[\w.-]+$/);
      expect(spec.binary).toBeTruthy();
    }
  });
});

describe("detectHostPlatform", () => {
  test("returns either supported pair or null", () => {
    const host = detectHostPlatform();
    if (host !== null) {
      expect(["darwin", "linux", "win32"]).toContain(host.platform);
      expect(["x64", "arm64"]).toContain(host.arch);
    }
  });
});
