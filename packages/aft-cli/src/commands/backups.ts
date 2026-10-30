import { forwardToNative } from "../lib/native-forward.js";

/**
 * Forward `backups <subcommand>` to the native binary. The Rust command owns
 * argument validation, the dry-run default, and routing the purge through a
 * running AFT daemon, so this wrapper adds no behavior of its own.
 */
export function runBackups(argv: string[]): number {
  return forwardToNative("backups", argv);
}
