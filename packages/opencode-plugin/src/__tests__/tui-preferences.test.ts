import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import { EventEmitter } from "node:events";
import * as fs from "node:fs";
import * as fsPromises from "node:fs/promises";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  computeEffectiveOrder,
  DEFAULT_PREFS,
  DEFAULT_SLOT_ORDER,
  getTuiPreferencesFile,
  PLUGIN_KEY,
  queueTuiPreferenceUpdate,
  readTuiPreferencesFile,
  resolveAftPrefs,
  TUI_PREFS_FILE_ENV,
  watchTuiPreferences,
} from "../tui/preferences";

let dir: string;
let file: string;
const savedEnv: Record<string, string | undefined> = {};
const ENV_KEYS = [TUI_PREFS_FILE_ENV, "OPENCODE_CONFIG_DIR", "XDG_CONFIG_HOME"];

beforeEach(async () => {
  for (const key of ENV_KEYS) savedEnv[key] = process.env[key];
  dir = await mkdtemp(join(tmpdir(), "aft-tui-prefs-test-"));
  file = join(dir, "tui-preferences.jsonc");
  process.env[TUI_PREFS_FILE_ENV] = file;
});

afterEach(async () => {
  for (const key of ENV_KEYS) {
    if (savedEnv[key] === undefined) delete process.env[key];
    else process.env[key] = savedEnv[key];
  }
  await rm(dir, { recursive: true, force: true });
});

describe("getTuiPreferencesFile", () => {
  test("env override wins", () => {
    expect(getTuiPreferencesFile()).toBe(file);
  });

  test("OPENCODE_CONFIG_DIR beats XDG_CONFIG_HOME", () => {
    delete process.env[TUI_PREFS_FILE_ENV];
    process.env.OPENCODE_CONFIG_DIR = "/cfg/opencode-dir";
    process.env.XDG_CONFIG_HOME = "/xdg";
    expect(getTuiPreferencesFile()).toBe("/cfg/opencode-dir/tui-preferences.jsonc");
  });

  test("XDG_CONFIG_HOME fallback appends opencode/", () => {
    delete process.env[TUI_PREFS_FILE_ENV];
    delete process.env.OPENCODE_CONFIG_DIR;
    process.env.XDG_CONFIG_HOME = "/xdg";
    expect(getTuiPreferencesFile()).toBe("/xdg/opencode/tui-preferences.jsonc");
  });
});

describe("readTuiPreferencesFile", () => {
  test("missing file returns empty object", async () => {
    expect(await readTuiPreferencesFile()).toEqual({});
  });

  test("parses JSONC with comments and trailing commas", async () => {
    await writeFile(
      file,
      `// header comment\n{\n  // plugin\n  "aft": { "order": 5, },\n}\n`,
      "utf8",
    );
    const root = await readTuiPreferencesFile();
    expect(root).toEqual({ aft: { order: 5 } });
  });

  test("malformed file returns empty object", async () => {
    await writeFile(file, "{{{{ not json", "utf8");
    expect(await readTuiPreferencesFile()).toEqual({});
  });

  test("non-object root returns empty object", async () => {
    await writeFile(file, "[1, 2, 3]", "utf8");
    expect(await readTuiPreferencesFile()).toEqual({});
  });
});

describe("resolveAftPrefs", () => {
  test("empty root yields defaults", () => {
    const prefs = resolveAftPrefs({});
    expect(prefs).toEqual(DEFAULT_PREFS);
    expect(prefs.order).toBe(DEFAULT_SLOT_ORDER);
    expect(prefs.collapsed).toBeNull();
  });

  test("valid values pass through", () => {
    const prefs = resolveAftPrefs({
      aft: {
        forceToTop: true,
        order: -500,
        startCollapsed: true,
        rememberCollapsed: false,
        collapsed: true,
        header: { label: "TOOLS", showVersion: false },
        sections: { searchIndex: false, compression: false },
      },
    });
    expect(prefs.forceToTop).toBe(true);
    expect(prefs.order).toBe(-500);
    expect(prefs.startCollapsed).toBe(true);
    expect(prefs.rememberCollapsed).toBe(false);
    expect(prefs.collapsed).toBe(true);
    expect(prefs.header).toEqual({ label: "TOOLS", showVersion: false });
    expect(prefs.sections).toEqual({
      searchIndex: false,
      semanticIndex: true,
      codeHealth: true,
      compression: false,
    });
  });

  test("numbers are clamped to their ranges", () => {
    const prefs = resolveAftPrefs({
      aft: { order: 99999999 },
    });
    expect(prefs.order).toBe(10000);
  });

  test("label is truncated to 20 chars and empty label falls back", () => {
    const long = resolveAftPrefs({
      aft: { header: { label: "X".repeat(50) } },
    });
    expect(long.header.label).toBe("X".repeat(20));
    const empty = resolveAftPrefs({
      aft: { header: { label: "" } },
    });
    expect(empty.header.label).toBe("AFT");
  });

  test("wrong types fall back per key", () => {
    const prefs = resolveAftPrefs({
      aft: {
        forceToTop: "yes",
        order: "high",
        collapsed: "maybe",
        header: "big",
        sections: { searchIndex: 1 },
      },
    });
    expect(prefs.forceToTop).toBe(false);
    expect(prefs.order).toBe(DEFAULT_SLOT_ORDER);
    expect(prefs.collapsed).toBeNull();
    expect(prefs.header).toEqual(DEFAULT_PREFS.header);
    expect(prefs.sections.searchIndex).toBe(true);
  });

  test("non-object plugin entry yields defaults", () => {
    expect(resolveAftPrefs({ aft: 42 })).toEqual(DEFAULT_PREFS);
  });

  test("partial object merges with defaults", () => {
    const prefs = resolveAftPrefs({
      aft: { sections: { semanticIndex: false } },
    });
    expect(prefs.sections.semanticIndex).toBe(false);
    expect(prefs.sections.searchIndex).toBe(true);
    expect(prefs.rememberCollapsed).toBe(true);
  });

  test("the prompt footer is on by default, off only for an explicit false", () => {
    expect(resolveAftPrefs({}).footer).toBe(true);
    expect(resolveAftPrefs({ aft: {} }).footer).toBe(true);
    expect(resolveAftPrefs({ aft: { footer: false } }).footer).toBe(false);
    expect(resolveAftPrefs({ aft: { footer: true } }).footer).toBe(true);
    for (const invalid of ["false", 0, null, {}, []]) {
      expect(resolveAftPrefs({ aft: { footer: invalid } }).footer).toBe(true);
    }
  });
});

describe("computeEffectiveOrder", () => {
  test("missing key returns default order", () => {
    expect(computeEffectiveOrder({}, "aft", DEFAULT_SLOT_ORDER)).toBe(DEFAULT_SLOT_ORDER);
  });

  test("explicit order knob is used and clamped", () => {
    expect(computeEffectiveOrder({ aft: { order: 42 } }, "aft", DEFAULT_SLOT_ORDER)).toBe(42);
    expect(computeEffectiveOrder({ aft: { order: -99999999 } }, "aft", DEFAULT_SLOT_ORDER)).toBe(
      -10000,
    );
  });

  test("forceToTop beats any explicit order", () => {
    expect(
      computeEffectiveOrder(
        { aft: { forceToTop: true, order: -10000 } },
        "aft",
        DEFAULT_SLOT_ORDER,
      ),
    ).toBe(-100000);
  });

  test("multiple forced plugins order by key position in file", () => {
    const root = {
      "plugin-a": { forceToTop: true },
      "plugin-b": { order: 5 },
      "plugin-c": { forceToTop: true },
    };
    expect(computeEffectiveOrder(root, "plugin-a", 0)).toBe(-100000);
    expect(computeEffectiveOrder(root, "plugin-c", 0)).toBe(-99998);
    expect(computeEffectiveOrder(root, "plugin-b", 0)).toBe(5);
  });

  test("non-boolean forceToTop is ignored", () => {
    expect(computeEffectiveOrder({ aft: { forceToTop: "yes" } }, "aft", DEFAULT_SLOT_ORDER)).toBe(
      DEFAULT_SLOT_ORDER,
    );
  });
});

describe("queueTuiPreferenceUpdate interop", () => {
  test("preference writes preserve invalid shared files and recover after rejection", async () => {
    for (const original of ['{"other": {', "[1, 2]", "null", ""]) {
      await writeFile(file, original);
      await expect(queueTuiPreferenceUpdate(PLUGIN_KEY, ["collapsed"], true)).rejects.toThrow();
      expect(await readFile(file, "utf8")).toBe(original);
    }
    await writeFile(file, '{"other": {"order": 42}}');
    await queueTuiPreferenceUpdate(PLUGIN_KEY, ["collapsed"], true);
    expect((await readTuiPreferencesFile()).other).toEqual({ order: 42 });
  });

  test("preference writes reject read errors without replacing the existing path", async () => {
    const original = '{"other": {"order": 42}}';
    await writeFile(file, original);
    const readMock = spyOn(fsPromises, "readFile").mockRejectedValueOnce(
      Object.assign(new Error("unreadable preferences"), { code: "EACCES" }),
    );
    try {
      await expect(queueTuiPreferenceUpdate(PLUGIN_KEY, ["collapsed"], true)).rejects.toThrow(
        "unreadable preferences",
      );
    } finally {
      readMock.mockRestore();
    }
    expect(await readFile(file, "utf8")).toBe(original);
  });

  test("preference writes initialize only missing shared files", async () => {
    await queueTuiPreferenceUpdate(PLUGIN_KEY, ["collapsed"], true);
    expect((await readTuiPreferencesFile()).aft).toEqual({ collapsed: true });
  });

  test("anthropic-auth values and comments survive aft-only collapsed update", async () => {
    const original = `// my shared notes
{
  // anthropic-auth plugin
  "anthropic-auth": {
    "pollMs": 2000, // tuned
    "collapsed": false
  },
  "aft": {
    "startCollapsed": false
  }
}
`;
    await writeFile(file, original, "utf8");
    await queueTuiPreferenceUpdate(PLUGIN_KEY, ["collapsed"], true);
    const text = await readFile(file, "utf8");
    const root = await readTuiPreferencesFile();
    expect(root["anthropic-auth"]).toEqual({ pollMs: 2000, collapsed: false });
    expect((root.aft as Record<string, unknown>).collapsed).toBe(true);
    expect(() => readTuiPreferencesFile()).not.toThrow();
    // comment-json round-trips sibling comments faithfully (verified): the
    // sibling key's values AND both its block and inline comments must survive
    // AFT writing only its own key.
    expect(text).toContain("anthropic-auth");
    expect(text).toContain("pollMs");
    expect(text).toContain("// my shared notes");
    expect(text).toContain("// anthropic-auth plugin");
    expect(text).toContain("// tuned");
  });
});

describe("watchTuiPreferences", () => {
  test("preference watcher handles asynchronous errors and closes only once", () => {
    let closes = 0;
    const watcher = Object.assign(new EventEmitter(), { close: () => closes++ });
    const watchMock = spyOn(fs, "watch").mockReturnValue(watcher as unknown as fs.FSWatcher);
    try {
      const stop = watchTuiPreferences(() => {});
      expect(() => watcher.emit("error", new Error("watch failed"))).not.toThrow();
      expect(closes).toBe(1);
      stop();
      expect(closes).toBe(1);
    } finally {
      watchMock.mockRestore();
    }
  });

  test("stopping preferences watcher suppresses an in-flight read callback", async () => {
    let onWatch: ((event: string, filename: string) => void) | undefined;
    let finishRead: ((text: string) => void) | undefined;
    let changes = 0;
    let reads = 0;
    let readStarted!: () => void;
    const started = new Promise<void>((resolve) => {
      readStarted = resolve;
    });
    const watcher = Object.assign(new EventEmitter(), { close: () => {} });
    const watchMock = spyOn(fs, "watch").mockImplementation(((
      _dir: unknown,
      callback: typeof onWatch,
    ) => {
      onWatch = callback;
      return watcher;
    }) as typeof fs.watch);
    const readMock = spyOn(fsPromises, "readFile").mockImplementation((() => {
      reads++;
      if (reads === 1) return Promise.resolve("old");
      readStarted();
      return new Promise<string>((resolve) => {
        finishRead = resolve;
      });
    }) as typeof fsPromises.readFile);
    let stop: (() => void) | undefined;
    try {
      stop = watchTuiPreferences(() => {
        changes++;
      });
      await Promise.resolve();
      onWatch?.("change", "tui-preferences.jsonc");
      await started;
      stop();
      finishRead?.("new");
      await new Promise((resolve) => setTimeout(resolve, 0));
      expect(changes).toBe(0);
    } finally {
      stop?.();
      watchMock.mockRestore();
      readMock.mockRestore();
    }
  });

  /**
   * Drives watchTuiPreferences with a fake directory watch and a scripted
   * readFile: each read takes the next entry, where a string is the file's
   * text and an error code makes the read fail with that code.
   */
  async function watchScripted(
    reads: Array<string | { code: string }>,
    events: number,
  ): Promise<number> {
    let onWatch: ((event: string, filename: string) => void) | undefined;
    const watcher = Object.assign(new EventEmitter(), { close: () => {} });
    const watchMock = spyOn(fs, "watch").mockImplementation(((
      _dir: unknown,
      callback: typeof onWatch,
    ) => {
      onWatch = callback;
      return watcher;
    }) as typeof fs.watch);
    let next = 0;
    const readMock = spyOn(fsPromises, "readFile").mockImplementation((() => {
      const result = reads[next++];
      if (result === undefined) throw new Error("more reads than scripted");
      if (typeof result === "string") return Promise.resolve(result);
      return Promise.reject(Object.assign(new Error(result.code), { code: result.code }));
    }) as typeof fsPromises.readFile);
    let changes = 0;
    const stop = watchTuiPreferences(() => {
      changes++;
    });
    try {
      await new Promise((resolve) => setTimeout(resolve, 0));
      for (let i = 0; i < events; i++) {
        onWatch?.("rename", "tui-preferences.jsonc");
        // Past the watcher's 150 ms debounce, so every event reads once.
        await new Promise((resolve) => setTimeout(resolve, 200));
      }
      expect(next).toBe(reads.length);
      return changes;
    } finally {
      stop();
      watchMock.mockRestore();
      readMock.mockRestore();
    }
  }

  test("deleting the file is one change, and recreating it is another", async () => {
    const changes = await watchScripted(
      ['{ "aft": { "footer": false } }', { code: "ENOENT" }, { code: "ENOENT" }, "{}"],
      3,
    );
    expect(changes).toBe(2);
  });

  test("a file missing at start is not reported as deleted again", async () => {
    const changes = await watchScripted([{ code: "ENOENT" }, { code: "ENOENT" }, "{}"], 2);
    expect(changes).toBe(1);
  });

  test("a present but unreadable file is not a change", async () => {
    const changes = await watchScripted(
      ['{ "aft": { "footer": false } }', { code: "EACCES" }, { code: "EISDIR" }],
      2,
    );
    expect(changes).toBe(0);
  });
});
