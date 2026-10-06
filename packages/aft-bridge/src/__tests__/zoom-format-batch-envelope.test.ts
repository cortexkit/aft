import { describe, expect, test } from "bun:test";
import {
  formatZoomText,
  isRustZoomBatchEnvelope,
  unwrapRustZoomBatchEnvelope,
} from "../zoom-format.js";

describe("Rust zoom batch envelope", () => {
  test("isRustZoomBatchEnvelope accepts valid batch shape", () => {
    const response = {
      success: true,
      complete: true,
      symbols: [
        { name: "a", response: { success: true, name: "a", content: "x" } },
        { name: "b", response: { success: false, message: "nope" } },
      ],
    };
    expect(isRustZoomBatchEnvelope(response)).toBe(true);
    expect(unwrapRustZoomBatchEnvelope(response)).toEqual({
      names: ["a", "b"],
      responses: [
        { success: true, name: "a", content: "x" },
        { success: false, message: "nope" },
      ],
    });
  });

  test("isRustZoomBatchEnvelope rejects single-symbol zoom shape", () => {
    expect(
      isRustZoomBatchEnvelope({
        success: true,
        name: "foo",
        content: "body",
      }),
    ).toBe(false);
  });
});

describe("formatZoomText call annotations", () => {
  test("renders other calls once with or without followable calls", () => {
    for (const calls of [[], [{ name: "lock", line: 2 }]]) {
      const text = formatZoomText("fixture.ts", {
        name: "A",
        kind: "function",
        content: "body",
        annotations: { calls_out: calls, other_calls: 3 },
      });
      const expectedCalls = calls.length ? "  lock (line 2)\n" : "";
      expect(text).toBe(
        `fixture.ts:1-1 [function A]\n\n1: body\n\n──── calls_out\n${expectedCalls}  +3 other calls`,
      );
    }
  });

  test("renders folded call-site counts compactly", () => {
    const text = formatZoomText("src/calls.ts", {
      name: "caller",
      kind: "function",
      range: { start_line: 10, end_line: 12 },
      content: `function caller() {
  helper();
}`,
      annotations: {
        calls_out: [{ name: "helper", line: 11, extra_count: 1 }],
        called_by: [{ name: "orchestrate", line: 20 }],
      },
    });

    expect(text).toContain("helper (line 11) +1");
    expect(text).toContain("orchestrate (line 20)");
  });
});
