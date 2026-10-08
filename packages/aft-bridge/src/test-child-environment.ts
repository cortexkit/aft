import { mkdirSync, realpathSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";

const DIRECTORIES = [
  "HOME",
  "USERPROFILE",
  "LOCALAPPDATA",
  "XDG_CONFIG_HOME",
  "XDG_DATA_HOME",
  "XDG_STATE_HOME",
  "XDG_CACHE_HOME",
  "XDG_RUNTIME_DIR",
  "AFT_STORAGE_DIR",
  "AFT_CACHE_DIR",
] as const;

const ROOTS = Symbol.for("cortexkit.aft.created-test-child-roots");
const TOOLCHAINS = Symbol.for("cortexkit.aft.test-child-toolchain-homes");
type ToolchainHomes = { RUSTUP_HOME: string; CARGO_HOME: string };

function toolchainHomes(inherited: NodeJS.ProcessEnv): ToolchainHomes {
  const global = globalThis as typeof globalThis & { [TOOLCHAINS]?: ToolchainHomes };
  if (!global[TOOLCHAINS]) {
    // Capture before suite setup replaces HOME. Source and dist imports must
    // share this snapshot rather than deriving toolchains from a fixture home.
    const home =
      process.platform === "win32"
        ? inherited.USERPROFILE || inherited.HOME || homedir()
        : inherited.HOME || homedir();
    global[TOOLCHAINS] = {
      RUSTUP_HOME: inherited.RUSTUP_HOME || join(home, ".rustup"),
      CARGO_HOME: inherited.CARGO_HOME || join(home, ".cargo"),
    };
  }
  return global[TOOLCHAINS];
}

function createdRoots(): Set<string> {
  // Harnesses import source, plugins import dist. Both must recognize the same
  // explicitly created fixture roots, even when os.homedir was cached earlier.
  const global = globalThis as typeof globalThis & { [ROOTS]?: Set<string> };
  return (global[ROOTS] ??= new Set());
}

/** Create every child directory before spawn, not lazily inside AFT. */
export function isolatedAftEnvironment(
  root: string,
  inherited: NodeJS.ProcessEnv = process.env,
): NodeJS.ProcessEnv {
  const env = { ...inherited };
  const toolchains = toolchainHomes(inherited);
  env.RUSTUP_HOME ||= toolchains.RUSTUP_HOME;
  env.CARGO_HOME ||= toolchains.CARGO_HOME;
  const names = [
    "home",
    "home",
    "local",
    "config",
    "data",
    "state",
    "cache",
    "runtime",
    "storage",
    "aft-cache",
  ];
  for (const [index, key] of DIRECTORIES.entries()) {
    env[key] = join(root, names[index]);
    mkdirSync(env[key], { recursive: true });
  }
  delete env.AFT_ALLOW_PRODUCTION_MIGRATION;
  createdRoots().add(resolve(root));
  return env;
}

function canonical(path: string): string {
  let ancestor = resolve(path);
  const tail: string[] = [];
  while (true) {
    try {
      return join(realpathSync(ancestor), ...tail);
    } catch {
      const parent = dirname(ancestor);
      if (parent === ancestor) return resolve(path);
      tail.unshift(relative(parent, ancestor));
      ancestor = parent;
    }
  }
}

function within(path: string, root: string): boolean {
  const tail = relative(canonical(root), canonical(path));
  return tail === "" || (!tail.startsWith("..") && !isAbsolute(tail));
}

/** Suite preloads enable this fence for shared spawns, including plugin dist.
 * Keep explicitly isolated fixtures, but never an ambient or caller-supplied
 * operator directory. The marker is absent in ordinary installed hosts.
 */
export function testChildEnvironment(env: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  const root = process.env.AFT_TEST_ISOLATION_ROOT;
  if (!root) return env;
  const fallback = isolatedAftEnvironment(root, env);
  for (const key of DIRECTORIES) {
    const value = env[key];
    if (
      value &&
      ([...createdRoots()].some((created) => within(value, created)) || within(value, tmpdir()))
    ) {
      mkdirSync(value, { recursive: true });
      fallback[key] = value;
    }
  }
  fallback.AFT_TEST_ISOLATION_ROOT = root;
  return fallback;
}
