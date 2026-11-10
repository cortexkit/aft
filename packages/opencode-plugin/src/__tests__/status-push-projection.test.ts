import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { coerceAftStatus } from "../shared/status.js";
import { formatAftStatusSegment } from "../tui/v2-status.js";

function fixture(name: string): Record<string, unknown> {
  return JSON.parse(
    readFileSync(
      new URL(
        `../../../../crates/aft/tests/fixtures/status_push_consumer_${name}.json`,
        import.meta.url,
      ),
      "utf8",
    ),
  ) as Record<string, unknown>;
}

describe("OpenCode 1 and 2 status push compatibility", () => {
  test("root-local projection preserves every normalized consumer field", () => {
    const full = fixture("sample");
    delete full.session;
    delete (full.compression as Record<string, unknown>).session;
    const snapshot = fixture("projected").snapshot as Record<string, unknown>;
    expect(coerceAftStatus(snapshot)).toEqual(coerceAftStatus(full));
    expect(snapshot.memory).toBeUndefined();
    expect(snapshot.session).toBeUndefined();
  });

  test("count-only backstop remains readable with fresh progress and safety state", () => {
    const frame = fixture("compacted");
    const snapshot = frame.snapshot as Record<string, unknown>;
    const status = coerceAftStatus(snapshot);
    expect(status.search_index.files).toBe(6200);
    expect(status.semantic_index.entries_done).toBe(40_000);
    expect(status.semantic_index.entries_total).toBe(52_000);
    expect(status.runtime.open_routes).toBe(90);
    expect(status.degraded).toBe(true);
    expect(status.degraded_reasons).toContain("root_too_broad");
    expect(status.status_bar?.diagnostics).toBe("producer_missing");
    expect(status.status_bar?.diagnostics_gaps).toEqual([
      { language: "rust", producer: "rust-analyzer", root: "/project" },
    ]);
    expect((snapshot.status_bar as Record<string, unknown>).disabled_categories).toEqual([
      "dead_code",
      "todos",
    ]);
    expect((snapshot.status_bar_values as Record<string, unknown>).disabled_categories).toEqual([
      "dead_code",
      "todos",
    ]);
    expect(formatAftStatusSegment(status)).toContain("T○");
    expect(snapshot).toMatchObject({
      storage_refusals_total: 19,
      storage_refusals_omitted: 19,
      storage_refusals_unattributed: 7,
      push_details: { omitted: true, read: "status" },
    });
    expect(
      (snapshot.status_push_list_totals as Record<string, unknown>)[
        "status_bar_values.diagnostics_gaps"
      ],
    ).toBe(1);
    expect(Buffer.byteLength(JSON.stringify(frame))).toBeLessThanOrEqual(16 * 1024);
  });
});
