/**
 * Format a token count for compact UI display.
 *
 * Examples: 42 -> "42", 5600 -> "5.6k", 1234567 -> "1.2M".
 *
 * Used in TUI sidebar and dialog rendering for compression aggregates.
 */
export function formatTokenCount(n: number): string {
  if (!Number.isFinite(n) || n < 0) return "0";
  if (n < 1_000) return String(Math.round(n));
  if (n < 1_000_000) return formatScaled(n / 1_000, "k");
  return formatScaled(n / 1_000_000, "M");
}

function formatScaled(value: number, suffix: string): string {
  const fixed = value.toFixed(1);
  return (fixed.endsWith(".0") ? fixed.slice(0, -2) : fixed) + suffix;
}

/**
 * Compute savings percent from original/compressed token counts.
 * Returns null when original is 0 (no division).
 */
export function compressionSavingsPercent(original: number, compressed: number): number | null {
  if (original <= 0) return null;
  const saved = Math.max(0, original - compressed);
  return Math.round((saved / original) * 100);
}
