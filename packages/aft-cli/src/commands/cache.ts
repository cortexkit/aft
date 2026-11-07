import { forwardToNative } from "../lib/native-forward.js";

/**
 * Forward `cache <subcommand>` to the native binary. The Rust command owns
 * argument validation, the dry-run default, the safety checks and every
 * printed line, so this wrapper adds no behavior of its own.
 */
export function runCache(argv: string[]): number {
  return forwardToNative("cache", argv);
}
