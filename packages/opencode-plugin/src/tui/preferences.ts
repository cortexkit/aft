import { watch } from "node:fs";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { basename, dirname, join } from "node:path";
import { parse, stringify } from "comment-json";

export const TUI_PREFS_FILE_ENV = "OPENCODE_TUI_PREFERENCES_FILE";
const FILE_NAME = "tui-preferences.jsonc";

export function getTuiPreferencesFile(): string {
  const override = process.env[TUI_PREFS_FILE_ENV];
  if (override) return override;
  const configDir =
    process.env.OPENCODE_CONFIG_DIR ||
    join(process.env.XDG_CONFIG_HOME || join(homedir(), ".config"), "opencode");
  return join(configDir, FILE_NAME);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export async function readTuiPreferencesFile(): Promise<Record<string, unknown>> {
  try {
    const raw = await readFile(getTuiPreferencesFile(), "utf8");
    const root: unknown = parse(raw);
    return isRecord(root) ? (root as Record<string, unknown>) : {};
  } catch {
    return {};
  }
}

export const PLUGIN_KEY = "aft";
export const DEFAULT_SLOT_ORDER = 180;

export interface AftTuiPrefs {
  forceToTop: boolean;
  order: number;
  startCollapsed: boolean;
  rememberCollapsed: boolean;
  collapsed: boolean | null;
  /**
   * Whether OpenCode 2 shows AFT's health line (`AFT E0 W0 | D0 U0 C0 | T0`)
   * in the prompt footer. Off hides it entirely; the sidebar and the status
   * dialog are unaffected.
   */
  footer: boolean;
  header: {
    label: string;
    showVersion: boolean;
  };
  sections: {
    searchIndex: boolean;
    semanticIndex: boolean;
    codeHealth: boolean;
    compression: boolean;
  };
}

export const DEFAULT_PREFS: AftTuiPrefs = {
  forceToTop: false,
  order: DEFAULT_SLOT_ORDER,
  startCollapsed: false,
  rememberCollapsed: true,
  collapsed: null,
  footer: true,
  header: { label: "AFT", showVersion: true },
  sections: {
    searchIndex: true,
    semanticIndex: true,
    codeHealth: true,
    compression: true,
  },
};

function bool(value: unknown, fallback: boolean): boolean {
  return typeof value === "boolean" ? value : fallback;
}

function int(value: unknown, fallback: number, min: number, max: number): number {
  if (typeof value !== "number" || !Number.isFinite(value)) return fallback;
  return Math.min(Math.max(Math.round(value), min), max);
}

function label(value: unknown, fallback: string, maxLength: number): string {
  if (typeof value !== "string" || value.length === 0) return fallback;
  return value.slice(0, maxLength);
}

/** Initial collapsed state from persisted prefs or startCollapsed default. */
export function seedCollapsedFromPrefs(prefs: AftTuiPrefs): boolean {
  return prefs.collapsed ?? prefs.startCollapsed;
}

export function resolveAftPrefs(root: Record<string, unknown>): AftTuiPrefs {
  const entry = root[PLUGIN_KEY];
  if (!isRecord(entry)) return structuredClone(DEFAULT_PREFS);

  const d = DEFAULT_PREFS;
  const header = isRecord(entry.header) ? entry.header : {};
  const sections = isRecord(entry.sections) ? entry.sections : {};

  return {
    forceToTop: bool(entry.forceToTop, d.forceToTop),
    order: int(entry.order, d.order, -10000, 10000),
    startCollapsed: bool(entry.startCollapsed, d.startCollapsed),
    rememberCollapsed: bool(entry.rememberCollapsed, d.rememberCollapsed),
    collapsed: typeof entry.collapsed === "boolean" ? entry.collapsed : null,
    footer: bool(entry.footer, d.footer),
    header: {
      label: label(header.label, d.header.label, 20),
      showVersion: bool(header.showVersion, d.header.showVersion),
    },
    sections: {
      searchIndex: bool(sections.searchIndex, d.sections.searchIndex),
      semanticIndex: bool(sections.semanticIndex, d.sections.semanticIndex),
      codeHealth: bool(sections.codeHealth, d.sections.codeHealth),
      compression: bool(sections.compression, d.sections.compression),
    },
  };
}

const FORCE_TOP_BASE = -100000;

export function computeEffectiveOrder(
  root: Record<string, unknown>,
  pluginKey: string,
  defaultOrder: number,
): number {
  const entry = root[pluginKey];
  if (!isRecord(entry)) return defaultOrder;
  if (entry.forceToTop === true) {
    return FORCE_TOP_BASE + Object.keys(root).indexOf(pluginKey);
  }
  return int(entry.order, defaultOrder, -10000, 10000);
}

const TEMPLATE = `// Shared preferences for opencode TUI plugins.
// One top-level key per plugin (short name). See each plugin's README for
// its supported settings. This file is safe to hand-edit; plugins update
// individual keys in place and preserve comments.
{}
`;

type JsonValue = string | number | boolean | null;

function setNested(target: Record<string, unknown>, path: string[], value: JsonValue): void {
  let current: Record<string, unknown> = target;
  for (let i = 0; i < path.length - 1; i++) {
    const key = path[i];
    if (key === undefined) return;
    const next = current[key];
    if (!isRecord(next)) {
      const created: Record<string, unknown> = {};
      current[key] = created;
      current = created;
    } else {
      current = next;
    }
  }
  const leaf = path[path.length - 1];
  if (leaf === undefined) return;
  current[leaf] = value;
}

async function writePreference(pluginKey: string, path: string[], value: JsonValue): Promise<void> {
  const file = getTuiPreferencesFile();
  await mkdir(dirname(file), { recursive: true });
  let text: string;
  try {
    text = await readFile(file, "utf8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    text = TEMPLATE;
  }
  // Only a missing file can be initialized. Invalid existing bytes may belong to another plugin.
  const parsed: unknown = parse(text);
  if (!isRecord(parsed)) throw new Error("TUI preferences must contain an object");
  const root = parsed;

  if (!isRecord(root[pluginKey])) {
    root[pluginKey] = {};
  }
  const entry = root[pluginKey] as Record<string, unknown>;
  setNested(entry, path, value);

  const next = `${stringify(root, null, 2)}\n`;
  const tmp = `${file}.${process.pid}.tmp`;
  await writeFile(tmp, next, "utf8");
  await rename(tmp, file);
}

let writeChain: Promise<void> = Promise.resolve();

export function queueTuiPreferenceUpdate(
  pluginKey: string,
  path: string[],
  value: JsonValue,
): Promise<void> {
  const update = writeChain.then(() => writePreference(pluginKey, path, value));
  // Keep subsequent writes usable while returning the actual failure to this caller.
  writeChain = update.catch(() => {});
  return update;
}

const WATCH_DEBOUNCE_MS = 150;

/**
 * What the watcher last saw where the preferences file should be: its text,
 * or FILE_ABSENT once a read has confirmed the file does not exist. A symbol
 * can never equal any file text, so deleting the file always counts as a
 * change, even from an empty file.
 */
const FILE_ABSENT: unique symbol = Symbol("tui-preferences file absent");
type WatchedState = string | typeof FILE_ABSENT;

/**
 * Read the file for the watcher. Returns FILE_ABSENT only for ENOENT; any
 * other failure (a permission error, a directory at the path) returns null,
 * meaning "unknown", so a file that is present but momentarily unreadable
 * never makes callers fall back to defaults.
 */
async function readWatchedState(file: string): Promise<WatchedState | null> {
  try {
    return await readFile(file, "utf8");
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "ENOENT" ? FILE_ABSENT : null;
  }
}

/**
 * Calls `onChange` after the preferences file's content changes, including
 * when the file is deleted (callers then read defaults) and when it is
 * created again.
 */
export function watchTuiPreferences(onChange: () => void): () => void {
  const file = getTuiPreferencesFile();
  const name = basename(file);
  let timer: ReturnType<typeof setTimeout> | null = null;
  // null until a read has succeeded or confirmed the file is missing.
  let lastSeen: WatchedState | null = null;
  let stopped = false;
  void readWatchedState(file).then((state) => {
    if (lastSeen === null && state !== null) lastSeen = state;
  });
  try {
    const watcher = watch(dirname(file), (_event, filename) => {
      const isOurs =
        filename === name || (filename?.startsWith(`${name}.`) && filename.endsWith(".tmp"));
      if (filename != null && !isOurs) return;
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => {
        timer = null;
        void readWatchedState(file).then((state) => {
          if (stopped || state === null) return;
          if (state === lastSeen) return;
          lastSeen = state;
          onChange();
        });
      }, WATCH_DEBOUNCE_MS);
    });
    const stop = () => {
      if (stopped) return;
      stopped = true;
      if (timer) clearTimeout(timer);
      timer = null;
      watcher.close();
    };
    watcher.on("error", stop);
    return stop;
  } catch {
    return () => {};
  }
}

export function persistCollapsedIfEnabled(prefs: AftTuiPrefs, collapsed: boolean): void {
  if (prefs.rememberCollapsed) {
    void queueTuiPreferenceUpdate(PLUGIN_KEY, ["collapsed"], collapsed).catch(() => {});
  }
}
