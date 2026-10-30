import { forwardToNative } from "../lib/native-forward.js";

/**
 * Run the native finite snapshot command. The npm CLI deliberately forwards no
 * root or index-selection behavior of its own; the Rust command validates that
 * `aft index` remains bare and schedules no future work.
 */
export function runIndex(argv: string[]): number {
  return forwardToNative("index", argv);
}
