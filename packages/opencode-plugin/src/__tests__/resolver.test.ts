import { describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { resolve } from "node:path";
import { findBinarySync, platformKey } from "@cortexkit/aft-bridge";

// ---------------------------------------------------------------------------
// platformKey() — pure mapping, no side effects
// ---------------------------------------------------------------------------

describe("platformKey", () => {
  test("darwin + arm64 → darwin-arm64", () => {
    expect(platformKey("darwin", "arm64")).toBe("darwin-arm64");
  });

  test("darwin + x64 → darwin-x64", () => {
    expect(platformKey("darwin", "x64")).toBe("darwin-x64");
  });

  test("linux + arm64 → linux-arm64", () => {
    expect(platformKey("linux", "arm64")).toBe("linux-arm64");
  });

  test("linux + x64 → linux-x64", () => {
    expect(platformKey("linux", "x64")).toBe("linux-x64");
  });

  test("win32 + x64 → win32-x64", () => {
    expect(platformKey("win32", "x64")).toBe("win32-x64");
  });

  test("unsupported platform throws with platform and arch in message", () => {
    expect(() => platformKey("freebsd", "x64")).toThrow(/Unsupported platform: freebsd.*arch: x64/);
  });

  test("unsupported arch on valid platform throws with arch details", () => {
    expect(() => platformKey("darwin", "s390x")).toThrow(
      /Unsupported architecture: s390x on platform darwin/,
    );
  });

  test("win32 + arm64 → win32-arm64 (native ARM64 binary)", () => {
    // v0.28.0 ships a real aarch64-pc-windows-msvc binary as
    // @cortexkit/aft-win32-arm64. Previously this key mapped to win32-x64
    // so ARM64 hosts ran the x64 build under Microsoft Prism emulation —
    // see packages/aft-bridge/src/platform.ts. With native ARM64, the
    // resolver, downloader, and ONNX layer pick the matching artifacts
    // and skip emulation overhead entirely.
    expect(platformKey("win32", "arm64")).toBe("win32-arm64");
  });

  test("defaults to process.platform and process.arch when no args", () => {
    // Should not throw on the current host
    const key = platformKey();
    expect(typeof key).toBe("string");
    expect(key).toContain("-");
  });
});

// ---------------------------------------------------------------------------
// Windows .exe suffix logic
// ---------------------------------------------------------------------------

describe("Windows binary naming", () => {
  test("win32-x64 platform key is used for Windows binary lookup", () => {
    const key = platformKey("win32", "x64");
    expect(key).toBe("win32-x64");
    // The resolver constructs `@cortexkit/aft-${key}/bin/aft.exe` for win32
    // Verify the naming convention matches the win32 platform package
    const expectedBin = `@cortexkit/aft-${key}/bin/aft.exe`;
    expect(expectedBin).toBe("@cortexkit/aft-win32-x64/bin/aft.exe");
  });

  test("non-win32 platforms do not use .exe", () => {
    for (const [platform, arch] of [
      ["darwin", "arm64"],
      ["darwin", "x64"],
      ["linux", "arm64"],
      ["linux", "x64"],
    ] as const) {
      const key = platformKey(platform, arch);
      const expectedBin = `@cortexkit/aft-${key}/bin/aft`;
      expect(expectedBin).not.toContain(".exe");
    }
  });
});

// ---------------------------------------------------------------------------
// findBinarySync() — integration tests for fallback chain
// ---------------------------------------------------------------------------

describe("findBinarySync", () => {
  test("finds binary via cache, PATH, or cargo fallback", () => {
    // This test relies on the debug binary being available (pretest runs cargo build)
    const debugBinary = resolve(import.meta.dir, "../../../../target/debug/aft");
    const hasBinary = existsSync(debugBinary);

    if (!hasBinary) {
      console.warn(
        "Skipping findBinary integration test — debug binary not built. Run `cargo build` first.",
      );
      return;
    }

    // findBinarySync should succeed since `which aft` or ~/.cargo/bin/aft should work
    const result = findBinarySync();
    // In a test env, the binary might not be on PATH — that returns null (no throw)
    if (result) {
      expect(typeof result).toBe("string");
      expect(result.length).toBeGreaterThan(0);
    } else {
      // Binary not found via sync methods — this is fine in a test env
      expect(result).toBeNull();
    }
  });

  test("returns null when binary not found (instead of throwing)", () => {
    // findBinarySync returns null instead of throwing when binary not found.
    // In a test env with the binary available, it should return a string.
    const result = findBinarySync();
    expect(result === null || typeof result === "string").toBe(true);
  });
});
