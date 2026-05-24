/**
 * Tests for `lsp-github-probe.ts`.
 *
 * Mirrors the npm registry probe test approach: feed synthetic releases
 * into `pickEligibleRelease` to verify the grace filter, ordering, and
 * draft/prerelease handling.
 */

import { describe, expect, test } from "bun:test";
import { assertSafeVersion, pickEligibleRelease, stripTagV } from "../lsp-github-probe.js";

const NOW = Date.parse("2025-01-15T12:00:00Z");
const DAY_MS = 24 * 60 * 60 * 1000;

function release(opts: {
  tag: string;
  ageDays: number;
  draft?: boolean;
  prerelease?: boolean;
  assets?: Array<{ name: string; browser_download_url: string }>;
}) {
  return {
    tag_name: opts.tag,
    published_at: new Date(NOW - opts.ageDays * DAY_MS).toISOString(),
    draft: opts.draft ?? false,
    prerelease: opts.prerelease ?? false,
    assets: opts.assets ?? [],
  };
}

describe("pickEligibleRelease", () => {
  test("picks the newest release older than graceDays", () => {
    const releases = [
      release({ tag: "v1.2.0", ageDays: 1 }), // too new
      release({ tag: "v1.1.0", ageDays: 10 }), // eligible
      release({ tag: "v1.0.0", ageDays: 30 }), // older eligible
    ];
    const result = pickEligibleRelease(releases, 7, NOW);
    expect(result.tag).toBe("v1.1.0");
    expect(result.blockedByGrace).toBe(false);
  });

  test("returns blockedByGrace=true when all releases are within grace", () => {
    const releases = [
      release({ tag: "v1.2.0", ageDays: 1 }),
      release({ tag: "v1.1.0", ageDays: 3 }),
    ];
    const result = pickEligibleRelease(releases, 7, NOW);
    expect(result.tag).toBeNull();
    expect(result.blockedByGrace).toBe(true);
  });

  test("returns null + blockedByGrace=false when no releases at all", () => {
    const result = pickEligibleRelease([], 7, NOW);
    expect(result.tag).toBeNull();
    expect(result.blockedByGrace).toBe(false);
  });

  test("skips drafts", () => {
    const releases = [
      release({ tag: "v1.2.0", ageDays: 30, draft: true }),
      release({ tag: "v1.1.0", ageDays: 10 }),
    ];
    const result = pickEligibleRelease(releases, 7, NOW);
    expect(result.tag).toBe("v1.1.0");
  });

  test("skips prereleases", () => {
    const releases = [
      release({ tag: "v2.0.0-rc.1", ageDays: 30, prerelease: true }),
      release({ tag: "v1.0.0", ageDays: 10 }),
    ];
    const result = pickEligibleRelease(releases, 7, NOW);
    expect(result.tag).toBe("v1.0.0");
  });

  test("includes asset list of chosen release", () => {
    const releases = [
      release({
        tag: "v1.0.0",
        ageDays: 10,
        assets: [
          { name: "binary-mac.zip", browser_download_url: "https://example.test/mac.zip" },
          { name: "binary-linux.tar.gz", browser_download_url: "https://example.test/lin.tar.gz" },
        ],
      }),
    ];
    const result = pickEligibleRelease(releases, 7, NOW);
    expect(result.tag).toBe("v1.0.0");
    expect(result.assets).toHaveLength(2);
    expect(result.assets[0]?.name).toBe("binary-mac.zip");
    expect(result.assets[0]?.url).toBe("https://example.test/mac.zip");
  });

  test("ignores releases without published_at", () => {
    const releases = [
      { tag_name: "v1.0.0", draft: false, prerelease: false } as any,
      release({ tag: "v0.9.0", ageDays: 15 }),
    ];
    const result = pickEligibleRelease(releases, 7, NOW);
    expect(result.tag).toBe("v0.9.0");
  });

  test("graceDays=0 makes any non-draft eligible", () => {
    const releases = [release({ tag: "v1.0.0", ageDays: 0.1 })];
    const result = pickEligibleRelease(releases, 0, NOW);
    expect(result.tag).toBe("v1.0.0");
  });
});

describe("stripTagV", () => {
  test("strips leading v", () => {
    expect(stripTagV("v1.2.3")).toBe("1.2.3");
  });
  test("leaves non-prefixed tags alone", () => {
    expect(stripTagV("1.2.3")).toBe("1.2.3");
    expect(stripTagV("21.1.0")).toBe("21.1.0");
  });
  test("does not strip multiple v's", () => {
    expect(stripTagV("vv1.0")).toBe("v1.0");
  });
  // Audit v0.17 #3 + #13: stripTagV must reject tag strings outside the
  // safe-version allowlist before they flow into paths or commands.
  test("rejects tags with shell metacharacters", () => {
    expect(() => stripTagV("1.0&calc.exe")).toThrow(/unsafe version\/tag string/);
    expect(() => stripTagV(`1.0"; rm -rf /; #`)).toThrow(/unsafe version\/tag string/);
    expect(() => stripTagV("1.0|cmd")).toThrow(/unsafe version\/tag string/);
    expect(() => stripTagV("1.0$(whoami)")).toThrow(/unsafe version\/tag string/);
  });
  test("rejects tags with path traversal", () => {
    expect(() => stripTagV("1.0/../etc")).toThrow(/unsafe version\/tag string/);
    expect(() => stripTagV("..\\..\\Windows")).toThrow(/unsafe version\/tag string/);
  });
  test("rejects tags with whitespace or null bytes", () => {
    expect(() => stripTagV("1.0 evil")).toThrow(/unsafe version\/tag string/);
    expect(() => stripTagV("1.0\nrm -rf /")).toThrow(/unsafe version\/tag string/);
    expect(() => stripTagV("1.0\x00")).toThrow(/unsafe version\/tag string/);
  });
});

describe("assertSafeVersion", () => {
  test("accepts conventional version formats", () => {
    expect(() => assertSafeVersion("1.2.3")).not.toThrow();
    expect(() => assertSafeVersion("v1.2.3")).not.toThrow();
    expect(() => assertSafeVersion("1.0.0-rc.1")).not.toThrow();
    expect(() => assertSafeVersion("1.0.0-beta+build.123")).not.toThrow();
  });
  test("rejects shell metacharacters and path traversal", () => {
    expect(() => assertSafeVersion("1.0&calc")).toThrow();
    expect(() => assertSafeVersion("../../etc")).toThrow();
    expect(() => assertSafeVersion("1.0;ls")).toThrow();
    expect(() => assertSafeVersion("1.0|cat")).toThrow();
  });
});
