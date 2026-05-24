import { describe, expect, test } from "bun:test";
import { compressionSavingsPercent, formatTokenCount } from "../format.js";

describe("compact formatting helpers", () => {
  test("formatTokenCount_zero", () => {
    expect(formatTokenCount(0)).toBe("0");
  });

  test("formatTokenCount_small", () => {
    expect(formatTokenCount(42)).toBe("42");
  });

  test("formatTokenCount_thousands", () => {
    expect(formatTokenCount(5600)).toBe("5.6k");
    expect(formatTokenCount(1234)).toBe("1.2k");
  });

  test("formatTokenCount_round_thousands", () => {
    expect(formatTokenCount(5000)).toBe("5k");
  });

  test("formatTokenCount_millions", () => {
    expect(formatTokenCount(1234567)).toBe("1.2M");
  });

  test("formatTokenCount_round_millions", () => {
    expect(formatTokenCount(2000000)).toBe("2M");
  });

  test("formatTokenCount_negative_or_NaN", () => {
    expect(formatTokenCount(-1)).toBe("0");
    expect(formatTokenCount(Number.NaN)).toBe("0");
  });

  test("compressionSavingsPercent_zero_original", () => {
    expect(compressionSavingsPercent(0, 0)).toBeNull();
  });

  test("compressionSavingsPercent_normal", () => {
    expect(compressionSavingsPercent(100, 60)).toBe(40);
  });

  test("compressionSavingsPercent_negative_savings_clamped", () => {
    expect(compressionSavingsPercent(50, 80)).toBe(0);
  });
});
