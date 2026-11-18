import { forwardToNative } from "../lib/native-forward.js";
import { runOldCacheCleanup } from "./clean-old.js";

/**
 * Forward cache maintenance to the native binary, retaining the cleanup
 * adapter's argument checks and launch-error reporting for `clean-old`.
 */
export function runCache(argv: string[]): number {
  if (argv[0] === "clean-old") return runOldCacheCleanup(argv.slice(1));
  return forwardToNative("cache", argv);
}
