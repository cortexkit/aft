/**
 * Live reload of AFT config file edits in a host plugin.
 *
 * The plugin keeps its own copy of the resolved config (`ctx.config`) for the
 * keys it enforces or uses itself, such as the path restriction pre-check and
 * the bash wait limits. The engine applies its own keys from the same files
 * separately, so this module only swaps the plugin's copy.
 *
 * Only keys that are safe to change under a running session are applied: the
 * ones the plugin reads from `ctx.config` on every tool call. Keys that change
 * the registered tools, their descriptions or the system text stay as loaded
 * and apply at the next host restart; they are reported as deferred.
 */

import { type FSWatcher, readFileSync, statSync, watch } from "node:fs";
import { basename, dirname } from "node:path";

/** How long a burst of file events must be quiet before the files are read. */
export const CONFIG_WATCH_DEBOUNCE_MS = 150;

/**
 * Appended to a config error found by a live reload, in place of the
 * "restart the host" note a startup config error carries: the plugin keeps
 * running on the last valid configuration.
 */
export const CONFIG_LIVE_KEEP_NOTE =
  "AFT keeps using the last valid configuration until the file is fixed.";

/** One config key the plugin applies live: how to read it and write it. */
export interface LiveConfigKey<C> {
  /** Dotted config key, as the user writes it in aft.jsonc. */
  name: string;
  read(config: C): unknown;
  /** Return a copy of `config` with this key set to `value`. */
  write(config: C, value: unknown): C;
}

/** The bash settings this module needs from a host's `resolveBashConfig`. */
export interface ResolvedBashForLiveReload {
  foreground_wait_window_ms: number;
  host_fallback: boolean;
  runon_enabled?: boolean;
  subagent_background: boolean;
  watch_sync_max_ms: number;
  worker_wait_max_ms: number;
}

type AnyConfig = Record<string, unknown>;

function pathKey<C>(name: string): LiveConfigKey<C> {
  const segments = name.split(".");
  return {
    name,
    read(config) {
      let value: unknown = config;
      for (const segment of segments) {
        if (value === null || typeof value !== "object") return undefined;
        value = (value as AnyConfig)[segment];
      }
      return value;
    },
    write(config, value) {
      const set = (target: unknown, index: number): AnyConfig => {
        const copy: AnyConfig =
          target !== null && typeof target === "object" && !Array.isArray(target)
            ? { ...(target as AnyConfig) }
            : {};
        const segment = segments[index] as string;
        if (index === segments.length - 1) {
          if (value === undefined) delete copy[segment];
          else copy[segment] = value;
        } else {
          copy[segment] = set(copy[segment], index + 1);
        }
        return copy;
      };
      return set(config, 0) as C;
    },
  };
}

/**
 * Bash keys are read through the host's `resolveBashConfig`, because `bash`
 * may be a boolean, an object or the legacy `experimental.bash` block. A write
 * turns `bash` into the equivalent object form, so every other bash setting
 * (for example `compress` or `background`, which are not live) resolves to
 * the same value as before.
 */
function bashKey<C>(
  name: keyof ResolvedBashForLiveReload,
  resolveBash: (config: C) => ResolvedBashForLiveReload & Record<string, unknown>,
): LiveConfigKey<C> {
  return {
    name: `bash.${name}`,
    read: (config) => resolveBash(config)[name],
    write(config, value) {
      const resolved: Record<string, unknown> = { ...resolveBash(config), [name]: value };
      for (const key of Object.keys(resolved)) {
        if (resolved[key] === undefined) delete resolved[key];
      }
      return { ...(config as AnyConfig), bash: resolved } as C;
    },
  };
}

/**
 * Every key the plugins read that a live reload applies. The engine-side
 * list is `apply_live_config` in `crates/aft/src/config_live.rs`.
 */
export function aftLiveConfigKeys<C>(
  resolveBash: (config: C) => ResolvedBashForLiveReload & Record<string, unknown>,
): LiveConfigKey<C>[] {
  return [
    pathKey<C>("configure_warnings_delivery"),
    pathKey<C>("restrict_to_project_root"),
    pathKey<C>("inspect.diagnostics_timeout_ms"),
    pathKey<C>("inspect.tier2_idle_minutes"),
    pathKey<C>("lsp.idle_minutes"),
    ...[
      "diagnostics",
      "todos",
      "dead_code",
      "unused_exports",
      "duplicates",
      "cycles",
      "complexity",
    ].map((category) => pathKey<C>(`inspect.categories.${category}`)),
    bashKey<C>("foreground_wait_window_ms", resolveBash),
    bashKey<C>("host_fallback", resolveBash),
    bashKey<C>("runon_enabled", resolveBash),
    bashKey<C>("subagent_background", resolveBash),
    bashKey<C>("watch_sync_max_ms", resolveBash),
    bashKey<C>("worker_wait_max_ms", resolveBash),
  ];
}

function sameValue(a: unknown, b: unknown): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

function flattenLeaves(value: unknown, prefix: string, out: Map<string, string>): void {
  if (value !== null && typeof value === "object" && !Array.isArray(value)) {
    const entries = Object.entries(value as AnyConfig);
    if (entries.length === 0 && prefix) out.set(prefix, "{}");
    for (const [key, child] of entries) {
      flattenLeaves(child, prefix ? `${prefix}.${key}` : key, out);
    }
    return;
  }
  if (prefix) out.set(prefix, JSON.stringify(value));
}

/** What {@link applyLiveConfigKeys} changed and what it left for a restart. */
export interface LiveConfigApply<C> {
  /** `current` with only the changed live keys taken from `next`. */
  config: C;
  applied: string[];
  deferred: string[];
  /** Security keys a project edit would have loosened; they stay as they were. */
  held?: string[];
}

/**
 * Copy the live keys that differ from `current` out of `next`. `config` is a
 * new object whenever something was applied, so a caller that captured the
 * old one keeps a consistent snapshot. Other settings that differ from
 * `baseline` (the config the plugin loaded at startup) are listed in
 * `deferred` and not applied.
 */
export function applyLiveConfigKeys<C>(
  current: C,
  next: C,
  keys: readonly LiveConfigKey<C>[],
  baseline: C = current,
): LiveConfigApply<C> {
  let config = current;
  const applied: string[] = [];
  for (const key of keys) {
    const value = key.read(next);
    if (sameValue(key.read(current), value)) continue;
    config = key.write(config, value);
    applied.push(key.name);
  }

  const before = new Map<string, string>();
  const after = new Map<string, string>();
  flattenLeaves(baseline, "", before);
  flattenLeaves(next, "", after);
  const liveNames = keys.map((key) => key.name);
  const isLive = (leaf: string): boolean =>
    liveNames.some((name) => leaf === name || leaf.startsWith(`${name}.`));
  const deferred = new Set<string>();
  for (const leaf of new Set([...before.keys(), ...after.keys()])) {
    if (before.get(leaf) === after.get(leaf) || isLive(leaf)) continue;
    deferred.add(leaf);
  }
  return { config, applied, deferred: [...deferred].sort() };
}

/** Options for {@link watchAftConfigFiles}. */
export interface WatchAftConfigFilesOptions {
  /** The config files to watch (user and project `aft.jsonc`). */
  paths: readonly string[];
  /**
   * Called after a quiet debounce window when any file's text changed.
   * Returning `false` means the new text was not accepted (for example a
   * half-written save): it is not recorded as seen, and the files are checked
   * again shortly even if no further event arrives. Returning a map records
   * the texts the callback actually accepted (path to text, `null` for an
   * absent file) instead of the texts this check read, which may differ if a
   * file changed in between.
   */
  onChange: () => unknown;
  debounceMs?: number;
  /**
   * The texts the caller already applied, keyed by path; a path that is not
   * listed was absent. When omitted, the texts are unknown, so the first
   * check calls `onChange`.
   */
  initialTexts?: Readonly<Record<string, string>>;
  /** Check the files once as soon as the watch is attached. */
  checkAtStart?: boolean;
  /** Test seam: replaces `fs.watch`. */
  watchImpl?: typeof watch;
  /** Test seam: runs between reading a directory's identity and watching it. */
  beforeWatchForTest?: (dir: string) => void;
}

function readTextOrNull(path: string): string | null {
  try {
    return readFileSync(path, "utf8");
  } catch {
    return null;
  }
}

/** How often the watches are checked against the directories on disk. */
const CONFIG_WATCH_REVALIDATE_MS = 2_000;
/** How many times a rejected text is re-checked without a new event. */
const CONFIG_WATCH_REJECTED_RETRIES = 3;
/** Longest a stream of events may postpone a check. */
const CONFIG_WATCH_MAX_DELAY_MS = 1_000;

/**
 * A directory's identity, which changes when the directory is replaced under
 * the same name.
 *
 * - Everywhere: device and inode. On Windows Node reports the volume serial
 *   number and the 64-bit file index there; `bigint` keeps the index exact.
 * - Not on Windows: also the birth time, because ext4 hands a freed inode
 *   number straight to the next directory created, so a directory deleted and
 *   recreated keeps its inode number while inotify's watch stays on the
 *   deleted one. On Windows the birth time is left out: NTFS file-name
 *   tunneling gives a name reused within about 15 s the creation time of the
 *   entry that last had it, so it carries no information there.
 *
 * Where a filesystem records no birth time it is 0 and the inode decides; the
 * delete event (see `watchAftConfigFiles`) re-arms the watch in that case.
 */
function identityOf(dir: string): string | null {
  try {
    const stat = statSync(dir, { bigint: true });
    const base = `${stat.dev}:${stat.ino}`;
    return process.platform === "win32" ? base : `${base}:${stat.birthtimeNs}`;
  } catch {
    return null;
  }
}

/**
 * The directory to watch for `file`: its own directory, or while that does
 * not exist, the nearest existing ancestor.
 */
function watchDirFor(file: string): string {
  let dir = dirname(file);
  while (dir !== dirname(dir) && identityOf(dir) === null) dir = dirname(dir);
  return dir;
}

/**
 * Watch config files the way editors save them: the parent directory is
 * watched, because an editor replaces the file by renaming a temporary
 * sibling over it. A directory that does not exist yet is watched through its
 * nearest existing ancestor until it appears, and a watch whose directory was
 * replaced or failed is re-armed. Every event in a watched directory wakes
 * the check (platforms may coalesce several changes into one event naming a
 * different entry), and a steady stream of events delays it at most
 * {@link CONFIG_WATCH_MAX_DELAY_MS}. `onChange` runs only when a file's text
 * actually differs from what was last seen. Returns a function that stops
 * every watch.
 */
export function watchAftConfigFiles(options: WatchAftConfigFilesOptions): () => void {
  const debounceMs = options.debounceMs ?? CONFIG_WATCH_DEBOUNCE_MS;
  // `undefined` for a path means "not known", which differs from every text.
  const lastSeen = new Map<string, string | null | undefined>();
  for (const path of options.paths) {
    if (options.initialTexts) lastSeen.set(path, options.initialTexts[path] ?? null);
    else lastSeen.set(path, options.checkAtStart ? undefined : readTextOrNull(path));
  }
  let timer: ReturnType<typeof setTimeout> | null = null;
  let firstPendingAt: number | null = null;
  let stopped = false;
  const watchers = new Map<
    string,
    { watcher: FSWatcher; identity: string | null; stale: boolean }
  >();

  const watchImpl = options.watchImpl ?? watch;
  let rejectedRetries = 0;
  const check = (): void => {
    timer = null;
    firstPendingAt = null;
    if (stopped) return;
    revalidate();
    const seen = new Map<string, string | null>();
    for (const path of options.paths) {
      const text = readTextOrNull(path);
      if (text !== lastSeen.get(path)) seen.set(path, text);
    }
    if (seen.size === 0) return;
    const outcome = options.onChange();
    if (outcome === false) {
      // Not accepted: keep the old texts as seen so the next event retries,
      // and retry on our own a few times in case no further event comes (an
      // editor that finished its save before this check read the file).
      if (rejectedRetries < CONFIG_WATCH_REJECTED_RETRIES) {
        rejectedRetries += 1;
        retryTimer = setTimeout(check, CONFIG_WATCH_MAX_DELAY_MS);
        retryTimer.unref?.();
      }
      return;
    }
    rejectedRetries = 0;
    if (outcome instanceof Map) {
      for (const path of options.paths) lastSeen.set(path, outcome.get(path) ?? null);
    } else {
      for (const [path, text] of seen) lastSeen.set(path, text);
    }
  };
  let retryTimer: ReturnType<typeof setTimeout> | null = null;
  const schedule = (): void => {
    if (stopped) return;
    const now = Date.now();
    firstPendingAt ??= now;
    if (timer) clearTimeout(timer);
    const delay = Math.max(
      0,
      Math.min(debounceMs, firstPendingAt + CONFIG_WATCH_MAX_DELAY_MS - now),
    );
    timer = setTimeout(check, delay);
    timer.unref?.();
  };

  /** Bring the watches in line with the directories on disk. */
  function revalidate(): boolean {
    const wanted = new Set(options.paths.map(watchDirFor));
    let moved = false;
    for (const [dir, entry] of watchers) {
      // A directory that is no longer wanted (its child appeared) or that was
      // replaced under the watch is dropped and, if still wanted, re-armed.
      if (!wanted.has(dir) || entry.stale || identityOf(dir) !== entry.identity) {
        entry.watcher.close();
        watchers.delete(dir);
        moved = true;
      }
    }
    for (const dir of wanted) {
      if (watchers.has(dir)) continue;
      try {
        // Read the identity first and confirm it after: a directory replaced
        // in between would leave the watch on the old one under the new
        // one's identity, and it would never be re-armed.
        const identity = identityOf(dir);
        options.beforeWatchForTest?.(dir);
        const name = basename(dir);
        const watcher = watchImpl(dir, { persistent: false }, (event, filename) => {
          // A `rename` naming the watched directory itself means the
          // directory was deleted or moved: the watch follows the old one,
          // so the next check re-arms it on whatever now has this path.
          if (event === "rename" && filename === name) {
            const current = watchers.get(dir);
            if (current && current.watcher === watcher) current.stale = true;
          }
          schedule();
        });
        if (identityOf(dir) !== identity) {
          watcher.close();
          moved = true;
          continue;
        }
        const entry = { watcher, identity, stale: false };
        entry.watcher.on("error", () => {
          entry.watcher.close();
          if (watchers.get(dir) === entry) watchers.delete(dir);
          // Re-arm on the next check rather than going silent.
          schedule();
        });
        watchers.set(dir, entry);
        moved = true;
      } catch {
        // The directory vanished between the check and the watch; the next
        // revalidation finds its nearest existing ancestor.
      }
    }
    return moved;
  }
  revalidate();
  if (options.checkAtStart) check();
  // Some platforms stop reporting on a deleted or replaced directory without
  // an error; a periodic check re-arms such a watch. Moving a watch (for
  // example once `.cortexkit/` is created) may reveal a file, so it checks.
  const revalidateTimer = setInterval(() => {
    if (!stopped && revalidate()) schedule();
  }, CONFIG_WATCH_REVALIDATE_MS);
  revalidateTimer.unref?.();

  return () => {
    stopped = true;
    if (timer) clearTimeout(timer);
    if (retryTimer) clearTimeout(retryTimer);
    clearInterval(revalidateTimer);
    for (const entry of watchers.values()) entry.watcher.close();
    watchers.clear();
  };
}

/**
 * Result of loading the config files for a live reload. `sources` lists the
 * config files the load actually read; a file it relied on last time that is
 * missing from it counts as deleted.
 */
export type LiveConfigLoad<C> =
  | {
      ok: true;
      config: C;
      sources: readonly string[];
      /** The project config file's text as this load read it, or null when absent. */
      projectText?: string | null;
      /** The text of each file this load read, keyed by path. */
      texts?: Readonly<Record<string, string>>;
    }
  | { ok: false; message: string };

/**
 * A security key and which of two values is the stricter one. A project-file
 * edit may move these only towards strict while the host runs.
 */
export interface LiveSecurityKey<C> {
  key: LiveConfigKey<C>;
  /** Whether changing the value from `current` to `next` loosens it. */
  loosens(current: unknown, next: unknown): boolean;
}

/** The plugin-read security keys: the path restriction and host fallback. */
export function aftLiveSecurityKeys<C>(keys: readonly LiveConfigKey<C>[]): LiveSecurityKey<C>[] {
  const byName = new Map(keys.map((key) => [key.name, key]));
  const pick = (name: string) => {
    const key = byName.get(name);
    if (!key) throw new Error(`unknown live config key ${name}`);
    return key;
  };
  return [
    {
      key: pick("restrict_to_project_root"),
      loosens: (current, next) => current === true && next !== true,
    },
    {
      key: pick("bash.runon_enabled"),
      loosens: (current, next) => current !== true && next === true,
    },
    {
      key: pick("bash.host_fallback"),
      loosens: (current, next) => current !== true && next === true,
    },
    {
      key: pick("lsp.idle_minutes"),
      loosens: (current, next) => {
        const minutes = (value: unknown) =>
          value === "never" ? Infinity : typeof value === "number" ? value : 60;
        return minutes(next) > minutes(current);
      },
    },
    ...[
      "diagnostics",
      "todos",
      "dead_code",
      "unused_exports",
      "duplicates",
      "cycles",
      "complexity",
    ].map((category) => ({
      key: pick(`inspect.categories.${category}`),
      loosens: (current: unknown, next: unknown) => current === false && next !== false,
    })),
  ];
}

/** Options for {@link startLiveConfigReload}. */
export interface LiveConfigReloadOptions<C> {
  /** The user and project `aft.jsonc` files. */
  paths: readonly string[];
  /** Load and resolve both files. Anything but a clean load is `ok: false`. */
  load(): LiveConfigLoad<C>;
  /** The config files the startup load that produced `getConfig()` read. */
  initialSources: readonly string[];
  /** The project config text the startup load read, or null when absent. */
  initialProjectText?: string | null;
  /** The text of each file the startup load read, keyed by path. */
  initialTexts?: Readonly<Record<string, string>>;
  /** Keys a project-file edit may only tighten; see {@link aftLiveSecurityKeys}. */
  securityKeys?: readonly LiveSecurityKey<C>[];
  keys: readonly LiveConfigKey<C>[];
  getConfig(): C;
  setConfig(config: C): void;
  /** Informational log line. */
  log(message: string): void;
  /** A config error, logged and shown to the user once per distinct message. */
  reportError(message: string): void;
  /** Skip the file watch; tests call `reload()` directly. */
  watch?: boolean;
  debounceMs?: number;
}

/** Handle for a running live config reload. */
export interface LiveConfigReload {
  /** Read the files now and apply the live keys that changed. */
  reload(): LiveConfigApply<unknown> | null;
  stop(): void;
}

/** The log line a reload writes, in the same shape as the engine's. */
export function liveConfigReloadLogLine(applied: string[], deferred: string[]): string {
  let line = "config reload";
  if (applied.length > 0) line += ` applied=[${applied.join(",")}]`;
  if (deferred.length > 0) {
    line += ` deferred=[${deferred.join(",")}] (deferred keys apply on next connect/restart)`;
  }
  return line;
}

/**
 * Watch the config files and keep `ctx.config` current for the live keys.
 * An invalid, unreadable or deleted file keeps the last valid config: the
 * error is reported and nothing else changes. Right after the watch starts
 * the files are read once, so an edit made while the host was starting (after
 * its config was loaded, before the watch existed) is not missed.
 */
export function startLiveConfigReload<C>(options: LiveConfigReloadOptions<C>): LiveConfigReload {
  const baseline = options.getConfig();
  let lastError: string | null = null;
  // The files the last accepted load read. A load that no longer reads one of
  // them keeps the last valid config rather than resolving the file as empty:
  // a security setting must not loosen because a file vanished mid-save or
  // was deleted by mistake. The next host restart applies a real deletion.
  let acceptedSources = new Set(options.initialSources);
  // The project text the published security values rest on. A project edit
  // that would loosen one of them is held until the next restart, and this
  // stays at the older text so later reloads keep holding it.
  let acceptedProjectText = options.initialProjectText ?? null;
  // The texts the last accepted load read, handed to the watch so it records
  // what was applied rather than what it happened to read.
  let acceptedTexts: Readonly<Record<string, string>> | undefined;
  const reload = (): LiveConfigApply<C> | null => {
    let loaded = options.load();
    if (loaded.ok) {
      const read = new Set(loaded.sources);
      const deleted = [...acceptedSources].find((path) => !read.has(path));
      if (deleted !== undefined) {
        loaded = { ok: false, message: `AFT config at ${deleted} was deleted` };
      } else {
        acceptedSources = read;
      }
    }
    if (!loaded.ok) {
      const message = `${loaded.message.trim().replace(/[.!?]?$/, ".")} ${CONFIG_LIVE_KEEP_NOTE}`;
      if (message !== lastError) {
        lastError = message;
        options.reportError(message);
      }
      return null;
    }
    lastError = null;
    acceptedTexts = loaded.texts;
    const current = options.getConfig();
    let next = loaded.config;
    const held: string[] = [];
    const projectText = loaded.projectText ?? null;
    if (loaded.projectText !== undefined && projectText !== acceptedProjectText) {
      for (const security of options.securityKeys ?? []) {
        const now = security.key.read(current);
        if (security.loosens(now, security.key.read(next))) {
          next = security.key.write(next, now);
          held.push(security.key.name);
        }
      }
      if (held.length === 0) acceptedProjectText = projectText;
    }
    const result = applyLiveConfigKeys(current, next, options.keys, baseline);
    if (result.applied.length > 0) options.setConfig(result.config);
    if (result.applied.length > 0 || result.deferred.length > 0 || held.length > 0) {
      let line = liveConfigReloadLogLine(result.applied, result.deferred);
      if (held.length > 0) {
        line += ` held=[${held.join(",")}] (not loosened while the project file differs from the one the published values came from; the next restart applies them)`;
      }
      options.log(line);
    }
    return { ...result, held };
  };
  const safeReload = (): false | ReadonlyMap<string, string | null> | true => {
    try {
      acceptedTexts = undefined;
      if (reload() === null) return false;
      const texts = acceptedTexts;
      if (!texts) return true;
      return new Map(options.paths.map((path) => [path, texts[path] ?? null]));
    } catch (err) {
      options.reportError(
        `AFT config reload failed: ${err instanceof Error ? err.message : String(err)}`,
      );
      return false;
    }
  };
  if (options.watch === false) {
    return { reload: reload as () => LiveConfigApply<unknown> | null, stop: () => {} };
  }
  // The first check runs as soon as the watch is attached, through the same
  // accepted-text and retry path as every later one, so an edit made while
  // the host was starting is applied and a transient rejection is retried.
  const stop = watchAftConfigFiles({
    paths: options.paths,
    debounceMs: options.debounceMs,
    onChange: safeReload,
    initialTexts: options.initialTexts,
    checkAtStart: true,
  });
  return { reload: reload as () => LiveConfigApply<unknown> | null, stop };
}
