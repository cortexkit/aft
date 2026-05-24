import { describe, expect, test } from "bun:test";
import { pickEligibleVersion } from "../lsp-registry-probe";

const DAY_MS = 24 * 60 * 60 * 1000;

function isoDaysAgo(now: number, days: number): string {
  return new Date(now - days * DAY_MS).toISOString();
}

describe("pickEligibleVersion", () => {
  test("returns the newest version older than the grace window", () => {
    const now = Date.parse("2026-04-27T00:00:00Z");
    const result = pickEligibleVersion(
      {
        time: {
          created: isoDaysAgo(now, 200),
          modified: isoDaysAgo(now, 1),
          "1.0.0": isoDaysAgo(now, 100),
          "1.1.0": isoDaysAgo(now, 50),
          "1.2.0": isoDaysAgo(now, 10),
          "1.3.0": isoDaysAgo(now, 3), // inside grace
        },
      },
      7,
      now,
    );
    expect(result.version).toBe("1.2.0");
    expect(result.blockedByGrace).toBe(false);
  });

  test("returns blockedByGrace when all candidates are inside the window", () => {
    const now = Date.parse("2026-04-27T00:00:00Z");
    const result = pickEligibleVersion(
      {
        time: {
          "1.0.0": isoDaysAgo(now, 2),
          "1.0.1": isoDaysAgo(now, 1),
        },
      },
      7,
      now,
    );
    expect(result.version).toBeNull();
    expect(result.blockedByGrace).toBe(true);
  });

  test("returns null when registry has no versions at all", () => {
    const result = pickEligibleVersion({ time: {} }, 7, Date.now());
    expect(result.version).toBeNull();
    expect(result.blockedByGrace).toBe(false);
  });

  test("skips pre-release versions even when they are old enough", () => {
    const now = Date.parse("2026-04-27T00:00:00Z");
    const result = pickEligibleVersion(
      {
        time: {
          "2.0.0-rc.1": isoDaysAgo(now, 60),
          "2.0.0-beta.5": isoDaysAgo(now, 30),
          "1.9.0": isoDaysAgo(now, 20),
        },
      },
      7,
      now,
    );
    expect(result.version).toBe("1.9.0");
  });

  test("ignores reserved keys 'created' and 'modified'", () => {
    const now = Date.parse("2026-04-27T00:00:00Z");
    const result = pickEligibleVersion(
      {
        time: {
          created: isoDaysAgo(now, 1000),
          modified: isoDaysAgo(now, 1),
          "1.0.0": isoDaysAgo(now, 30),
        },
      },
      7,
      now,
    );
    expect(result.version).toBe("1.0.0");
    expect(result.eligible).toHaveLength(1);
  });

  test("graceDays=0 selects the newest stable version", () => {
    const now = Date.parse("2026-04-27T00:00:00Z");
    const result = pickEligibleVersion(
      {
        time: {
          "1.0.0": isoDaysAgo(now, 1),
          "1.1.0": isoDaysAgo(now, 0.1),
        },
      },
      0,
      now,
    );
    expect(result.version).toBe("1.1.0");
  });

  test("ignores entries with non-string values defensively", () => {
    const now = Date.parse("2026-04-27T00:00:00Z");
    const result = pickEligibleVersion(
      {
        time: {
          "1.0.0": isoDaysAgo(now, 30),
          "1.1.0": 12345 as any,
        },
      },
      7,
      now,
    );
    expect(result.version).toBe("1.0.0");
  });
});
