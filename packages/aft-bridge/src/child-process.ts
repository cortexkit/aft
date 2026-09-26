/**
 * The only place in AFT's TypeScript packages that starts child processes.
 *
 * On Windows a console program started by a parent that has no console of its
 * own (for example a host running as a background service) is given a brand
 * new console window unless the spawn asks for it to be hidden. Node's
 * `spawnSync`, `execSync`, `execFileSync` and friends leave `windowsHide`
 * false by default, so every forgotten option turned into a visible terminal
 * window that stole focus from the user. These wrappers have the same
 * signatures as their `node:child_process` namesakes and always force
 * `windowsHide: true`; the option has no effect on other platforms.
 *
 * A source-scanning test rejects any other module that imports
 * `node:child_process` or calls `Bun.spawn`, so new call sites have to come
 * through here.
 */

import type * as childProcess from "node:child_process";
import {
  execFileSync as nodeExecFileSync,
  execSync as nodeExecSync,
  spawn as nodeSpawn,
  spawnSync as nodeSpawnSync,
} from "node:child_process";

export type {
  ChildProcess,
  ExecFileSyncOptions,
  ExecSyncOptions,
  SpawnOptions,
  SpawnSyncOptions,
  SpawnSyncReturns,
} from "node:child_process";

const HIDDEN = { windowsHide: true } as const;

/**
 * Return a copy of `args` whose options object carries `windowsHide: true`.
 *
 * `takesArgv` covers the `(file, args?, options?, callback?)` shape used by
 * `spawn`, `spawnSync` and `execFileSync`, where the argv array may be
 * omitted; without it the options sit directly after the command, as for
 * `execSync(command, options?)`. When the caller passed no options object,
 * one is inserted in the slot Node expects.
 */
export function withWindowsHidden(args: readonly unknown[], takesArgv: boolean): unknown[] {
  const out = [...args];
  let index = 1;
  if (takesArgv && (Array.isArray(out[1]) || (out[1] == null && out.length > 2))) index = 2;
  const options = out[index];
  if (options == null) {
    out[index] = { ...HIDDEN };
  } else if (typeof options === "function") {
    // A trailing callback sits where the options would go; put them before it.
    out.splice(index, 0, { ...HIDDEN });
  } else if (typeof options === "object") {
    out[index] = { ...(options as object), ...HIDDEN };
  }
  return out;
}

type AnyFunction = (...args: unknown[]) => unknown;

/**
 * Wrap one `node:child_process` function. `resolve` is called on every
 * invocation rather than once, so the live import binding is used and a test
 * that replaces the module still sees its replacement called.
 */
function hidden<T>(resolve: () => T, takesArgv: boolean): T {
  return ((...args: unknown[]) =>
    (resolve() as unknown as AnyFunction)(...withWindowsHidden(args, takesArgv))) as T;
}

/** `child_process.spawn` with `windowsHide: true` forced. */
export const spawn: typeof childProcess.spawn = hidden(() => nodeSpawn, true);

/** `child_process.spawnSync` with `windowsHide: true` forced. */
export const spawnSync: typeof childProcess.spawnSync = hidden(() => nodeSpawnSync, true);

/** `child_process.execFileSync` with `windowsHide: true` forced. */
export const execFileSync: typeof childProcess.execFileSync = hidden(() => nodeExecFileSync, true);

/** `child_process.execSync` with `windowsHide: true` forced. */
export const execSync: typeof childProcess.execSync = hidden(() => nodeExecSync, false);
