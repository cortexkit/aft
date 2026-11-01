/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import {
  mkdirSync,
  mkdtempSync,
  renameSync,
  rmSync,
  statSync,
  symlinkSync,
  utimesSync,
  watch,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  aftLiveConfigKeys,
  applyLiveConfigKeys,
  CONFIG_LIVE_KEEP_NOTE,
  type LiveConfigLoad,
  liveConfigReloadLogLine,
  type ResolvedBashForLiveReload,
  startLiveConfigReload,
  watchAftConfigFiles,
} from "../config-watch.js";

type TestConfig = {
  lsp?: { idle_minutes?: number | "never" };
  restrict_to_project_root?: boolean;
  disabled_tools?: string[];
  bash?: boolean | Record<string, unknown>;
  inspect?: Record<string, unknown>;
};

/** A small stand-in for a host's `resolveBashConfig`. */
function resolveBash(config: TestConfig): ResolvedBashForLiveReload & Record<string, unknown> {
  const top = config.bash;
  const object = typeof top === "object" && top !== null ? top : {};
  return {
    enabled: top !== false,
    compress: top === true || (object.compress ?? true) === true,
    background: top === true || (object.background ?? true) === true,
    host_fallback: object.host_fallback === true,
    subagent_background: object.subagent_background !== false,
    foreground_wait_window_ms: (object.foreground_wait_window_ms as number) ?? 15_000,
    watch_sync_max_ms: (object.watch_sync_max_ms as number) ?? 120_000,
    worker_wait_max_ms: (object.worker_wait_max_ms as number) ?? 1_800_000,
    env: (object.env as Record<string, string>) ?? {},
  };
}

const KEYS = aftLiveConfigKeys<TestConfig>(resolveBash);
const tempDirs: string[] = [];

afterEach(() => {
  for (const dir of tempDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function tempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), "aft-config-watch-"));
  tempDirs.push(dir);
  return dir;
}

describe("applyLiveConfigKeys", () => {
  test("inspect categories and LSP idle minutes apply live without deferral", () => {
    const current: TestConfig = {
      lsp: { idle_minutes: 60 },
      inspect: { categories: { dead_code: true } },
    };
    const next: TestConfig = {
      lsp: { idle_minutes: "never" },
      inspect: { categories: { dead_code: false } },
    };
    const result = applyLiveConfigKeys(current, next, KEYS);
    expect(result.applied).toContain("lsp.idle_minutes");
    expect(result.applied).toContain("inspect.categories.dead_code");
    expect(result.deferred).toEqual([]);
    expect(result.config).toEqual(next);
    expect(current.lsp?.idle_minutes).toBe(60);
  });
  test("applies a live key and defers the rest", () => {
    const current: TestConfig = { restrict_to_project_root: false, disabled_tools: [] };
    const next: TestConfig = { restrict_to_project_root: true, disabled_tools: ["aft_zoom"] };

    const result = applyLiveConfigKeys(current, next, KEYS);

    expect(result.applied).toEqual(["restrict_to_project_root"]);
    expect(result.deferred).toEqual(["disabled_tools"]);
    expect(result.config.restrict_to_project_root).toBe(true);
    expect(result.config.disabled_tools).toEqual([]);
    // The caller's old snapshot is not mutated.
    expect(current.restrict_to_project_root).toBe(false);
  });
  test("bash.env changes apply to future commands without a reconnect", () => {
    const current: TestConfig = { bash: { env: { TZ: "UTC" } } };
    const next: TestConfig = { bash: { env: { TZ: "Etc/GMT", LANG: "C.UTF-8" } } };
    const result = applyLiveConfigKeys(current, next, KEYS);
    expect(result.applied).toContain("bash.env");
    expect(result.deferred).toEqual([]);
    expect(result.config).toMatchObject(next);
  });

  test("a live bash key keeps every other bash setting as loaded", () => {
    const current: TestConfig = { bash: true };
    const next: TestConfig = { bash: { watch_sync_max_ms: 5_000, compress: false } };

    const result = applyLiveConfigKeys(current, next, KEYS);

    expect(result.applied).toEqual(["bash.watch_sync_max_ms"]);
    const bash = resolveBash(result.config);
    expect(bash.watch_sync_max_ms).toBe(5_000);
    expect(bash.compress).toBe(true);
    expect(bash.background).toBe(true);
  });

  test("the log line names applied and deferred keys", () => {
    expect(liveConfigReloadLogLine(["restrict_to_project_root"], ["disabled_tools"])).toBe(
      "config reload applied=[restrict_to_project_root] deferred=[disabled_tools] (deferred keys apply on next connect/restart)",
    );
  });
});

describe("startLiveConfigReload", () => {
  function harness(
    initial: TestConfig,
    initialSources: string[] = [],
    projectPath = "/cfg/project/aft.jsonc",
    initialProjectText?: string,
  ) {
    let config = initial;
    let nextLoad: LiveConfigLoad<TestConfig> = {
      ok: true,
      config: initial,
      sources: initialSources,
    };
    const errors: string[] = [];
    const logs: string[] = [];
    const reload = startLiveConfigReload<TestConfig>({
      paths: [],
      projectPath,
      load: () => nextLoad,
      initialSources,
      initialProjectText,
      keys: KEYS,
      getConfig: () => config,
      setConfig: (next) => {
        config = next;
      },
      log: (line) => logs.push(line),
      reportError: (message) => errors.push(message),
      watch: false,
    });
    return {
      reload,
      errors,
      logs,
      config: () => config,
      loads: (load: LiveConfigLoad<TestConfig>) => {
        nextLoad = load;
      },
    };
  }

  test("an invalid file keeps the last valid config and is reported once", () => {
    const h = harness({ restrict_to_project_root: true });
    h.loads({ ok: false, message: "AFT config at /x failed to parse: bad" });

    h.reload.reload();
    h.reload.reload();

    expect(h.config().restrict_to_project_root).toBe(true);
    expect(h.errors).toEqual([`AFT config at /x failed to parse: bad. ${CONFIG_LIVE_KEEP_NOTE}`]);
  });

  test("a load that no longer read a file it relied on keeps the last valid config", () => {
    // The file vanished before the loader looked, so the loader resolved it as
    // absent without any error; only its record of what it read tells.
    const h = harness({ restrict_to_project_root: true }, ["/cfg/user/aft.jsonc"]);
    h.loads({ ok: true, config: { restrict_to_project_root: false }, sources: [] });

    h.reload.reload();

    expect(h.config().restrict_to_project_root).toBe(true);
    expect(h.errors[0]).toContain("/cfg/user/aft.jsonc was deleted");
  });

  test("a missing project file removes its live settings", () => {
    const projectPath = "/cfg/project/aft.jsonc";
    const h = harness({ restrict_to_project_root: true }, [projectPath], projectPath, "{}");
    h.loads({
      ok: true,
      config: { restrict_to_project_root: false },
      sources: [],
      projectText: null,
    });

    h.reload.reload();

    expect(h.config().restrict_to_project_root).toBe(false);
    expect(h.errors).toEqual([]);
  });

  test("a file absent at startup may appear later", () => {
    const h = harness({ restrict_to_project_root: false }, []);
    h.loads({
      ok: true,
      config: { restrict_to_project_root: true },
      sources: ["/cfg/user/aft.jsonc"],
    });
    h.reload.reload();
    expect(h.config().restrict_to_project_root).toBe(true);
  });

  test("a valid edit swaps in a new config object", () => {
    const initial: TestConfig = { restrict_to_project_root: false };
    const h = harness(initial);
    h.loads({ ok: true, config: { restrict_to_project_root: true }, sources: [] });

    h.reload.reload();

    expect(h.config()).not.toBe(initial);
    expect(h.config().restrict_to_project_root).toBe(true);
    expect(h.logs).toEqual(["config reload applied=[restrict_to_project_root]"]);
  });
});

describe("watchAftConfigFiles", () => {
  test("a symlink target edit reloads without an event in the link folder", async () => {
    const dir = tempDir();
    const configDir = join(dir, "cortexkit");
    const dotfiles = join(dir, "dotfiles");
    mkdirSync(configDir);
    mkdirSync(dotfiles);
    const file = join(configDir, "aft.jsonc");
    const target = join(dotfiles, "settings.jsonc");
    writeFileSync(target, "{}");
    symlinkSync(target, file, "file");
    let changes = 0;
    const watched: string[] = [];
    const stop = watchAftConfigFiles({
      paths: [file],
      debounceMs: 20,
      watchImpl: ((
        path: string,
        options: { persistent: boolean },
        listener: (event: string, filename: string | null) => void,
      ) => {
        watched.push(String(path));
        return watch(path, options, listener);
      }) as unknown as typeof watch,
      onChange: () => {
        changes += 1;
      },
    });
    try {
      expect(watched).toContain(dotfiles);
      writeFileSync(target, '{ "bash": { "env": { "TEST": "edited" } } }');
      await waitUntil(() => changes >= 1, "symlink target edit reloaded");

      const replacement = join(dotfiles, "save.jsonc");
      writeFileSync(replacement, '{ "bash": { "env": { "TEST": "atomic" } } }');
      renameSync(replacement, target);
      await waitUntil(() => changes >= 2, "rename-over target reloaded");
      rmSync(target);
      await waitUntil(() => changes >= 3, "missing target noticed");
      writeFileSync(target, "{}");
      await waitUntil(() => changes >= 4, "missing target recovered");

      const other = join(dir, "other-dotfiles");
      mkdirSync(other);
      const alternate = join(other, "alternate.jsonc");
      writeFileSync(alternate, '{ "bash": { "env": { "TEST": "retargeted" } } }');
      rmSync(file);
      symlinkSync(alternate, file, "file");
      await waitUntil(() => changes >= 5, "retargeted link reloaded");
      expect(watched).toContain(other);
      writeFileSync(alternate, '{ "bash": { "env": { "TEST": "edited again" } } }');
      await waitUntil(() => changes >= 6, "new target edit reloaded");
    } finally {
      stop();
    }
  });

  test("a symlinked config directory watches its resolved target", async () => {
    const dir = tempDir();
    const dotfiles = join(dir, "dotfiles");
    mkdirSync(dotfiles);
    const target = join(dotfiles, "aft.jsonc");
    writeFileSync(target, "{}");
    const configDir = join(dir, "cortexkit");
    symlinkSync(dotfiles, configDir, "dir");
    let changes = 0;
    const stop = watchAftConfigFiles({
      paths: [join(configDir, "aft.jsonc")],
      debounceMs: 20,
      onChange: () => {
        changes += 1;
      },
    });
    try {
      writeFileSync(target, '{ "bash": { "enabled": false } }');
      await waitUntil(() => changes >= 1, "target under a directory symlink reloaded");
    } finally {
      stop();
    }
  });

  test("an unwatchable symlink target is polled even when size and mtime match", async () => {
    const dir = tempDir();
    const dotfiles = join(dir, "dotfiles");
    const configDir = join(dir, "cortexkit");
    mkdirSync(dotfiles);
    mkdirSync(configDir);
    const file = join(configDir, "aft.jsonc");
    const target = join(dotfiles, "settings.jsonc");
    writeFileSync(target, '{ "value": "before" }');
    const metadata = statSync(target);
    symlinkSync(target, file, "file");
    let changes = 0;
    let attempted = false;
    const stop = watchAftConfigFiles({
      paths: [file],
      debounceMs: 20,
      watchImpl: ((
        path: string,
        options: { persistent: boolean },
        listener: (event: string, filename: string | null) => void,
      ) => {
        if (String(path) === dotfiles) {
          attempted = true;
          throw new Error("target watch unavailable");
        }
        return watch(path, options, listener);
      }) as unknown as typeof watch,
      onChange: () => {
        changes += 1;
      },
    });
    try {
      expect(attempted).toBe(true);
      writeFileSync(target, '{ "value": "after!" }');
      utimesSync(target, metadata.atime, metadata.mtime);
      await waitUntil(() => changes >= 1, "unwatchable target edit reloaded");
    } finally {
      stop();
    }
  });

  test("an edit, and a config directory created later, call onChange", async () => {
    const dir = tempDir();
    const file = join(dir, ".cortexkit", "aft.jsonc");
    let changes = 0;
    const stop = watchAftConfigFiles({
      paths: [file],
      debounceMs: 20,
      onChange: () => {
        changes += 1;
      },
    });
    const waitFor = async (count: number) => {
      const deadline = Date.now() + 5_000;
      while (changes < count && Date.now() < deadline) {
        await new Promise((resolve) => setTimeout(resolve, 25));
      }
      expect(changes).toBeGreaterThanOrEqual(count);
    };
    try {
      mkdirSync(join(dir, ".cortexkit"));
      writeFileSync(file, "{}");
      await waitFor(1);
      await new Promise((resolve) => setTimeout(resolve, 100));
      const seen = changes;
      writeFileSync(file, '{ "restrict_to_project_root": true }');
      await waitFor(seen + 1);
    } finally {
      stop();
    }
  });
});

async function waitUntil(
  predicate: () => boolean,
  label: string,
  timeoutMs = 5_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error(`timed out: ${label}`);
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
}

describe("watchAftConfigFiles recovery", () => {
  test("a replaced config directory is watched again", async () => {
    const dir = tempDir();
    const configDir = join(dir, ".cortexkit");
    const file = join(configDir, "aft.jsonc");
    mkdirSync(configDir);
    writeFileSync(file, "{}");
    let changes = 0;
    const stop = watchAftConfigFiles({
      paths: [file],
      debounceMs: 20,
      onChange: () => {
        changes += 1;
      },
    });
    try {
      rmSync(configDir, { recursive: true, force: true });
      await waitUntil(() => changes >= 1, "deletion noticed");
      mkdirSync(configDir);
      writeFileSync(file, '{ "restrict_to_project_root": true }');
      await waitUntil(() => changes >= 2, "recreated file noticed");
      await new Promise((resolve) => setTimeout(resolve, 300));
      const seen = changes;
      writeFileSync(file, '{ "restrict_to_project_root": false }');
      await waitUntil(() => changes > seen, "edit in the recreated directory noticed");
    } finally {
      stop();
    }
  });

  test("steady unrelated activity in the directory does not starve a config edit", async () => {
    const dir = tempDir();
    const file = join(dir, "aft.jsonc");
    writeFileSync(file, "{}");
    let changes = 0;
    const stop = watchAftConfigFiles({
      paths: [file],
      debounceMs: 150,
      onChange: () => {
        changes += 1;
      },
    });
    let n = 0;
    const noise = setInterval(() => writeFileSync(join(dir, "status"), String(n++)), 40);
    try {
      writeFileSync(file, '{ "restrict_to_project_root": true }');
      await waitUntil(() => changes >= 1, "config edit applied under noise", 3_000);
    } finally {
      clearInterval(noise);
      stop();
    }
  });
});

describe("startLiveConfigReload start-up", () => {
  test("an edit made before the watch started is applied at once", () => {
    let config: TestConfig = { restrict_to_project_root: false };
    const reload = startLiveConfigReload<TestConfig>({
      paths: [join(tempDir(), "aft.jsonc")],
      // The host loaded its config before this edit; the file now says true.
      load: () => ({ ok: true, config: { restrict_to_project_root: true }, sources: [] }),
      initialSources: [],
      keys: KEYS,
      getConfig: () => config,
      setConfig: (next) => {
        config = next;
      },
      log: () => {},
      reportError: () => {},
    });
    try {
      expect(config.restrict_to_project_root).toBe(true);
    } finally {
      reload.stop();
    }
  });
});

describe("startLiveConfigReload accepted texts", () => {
  test("a transient rejection at start-up is retried", async () => {
    const file = join(tempDir(), "aft.jsonc");
    writeFileSync(file, '{ "restrict_to_project_root": true }');
    let config: TestConfig = { restrict_to_project_root: false };
    let loads = 0;
    const reload = startLiveConfigReload<TestConfig>({
      paths: [file],
      // The first load catches the file mid-save; the next one succeeds.
      load: () =>
        loads++ === 0
          ? { ok: false, message: "AFT config at x failed to parse: truncated" }
          : { ok: true, config: { restrict_to_project_root: true }, sources: [file] },
      initialSources: [],
      keys: KEYS,
      getConfig: () => config,
      setConfig: (next) => {
        config = next;
      },
      log: () => {},
      reportError: () => {},
    });
    try {
      await waitUntil(() => config.restrict_to_project_root === true, "start-up retry", 4_000);
    } finally {
      reload.stop();
    }
  });

  test("the watch records the text the load accepted, not the text it read", async () => {
    const dir = tempDir();
    const file = join(dir, "aft.jsonc");
    writeFileSync(file, "{}");
    let calls = 0;
    const stop = watchAftConfigFiles({
      paths: [file],
      debounceMs: 20,
      initialTexts: { [file]: "{}" },
      // The load read an older text than the check did.
      onChange: () => {
        calls += 1;
        return new Map([[file, "{}"]]);
      },
    });
    try {
      await new Promise((resolve) => setTimeout(resolve, 300));
      writeFileSync(file, '{ "restrict_to_project_root": true }');
      await waitUntil(() => calls >= 1, "first check", 4_000);
      // An unrelated event wakes the check again: the file still differs
      // from what was accepted, so it is loaded again.
      writeFileSync(join(dir, "status"), "1");
      await waitUntil(() => calls >= 2, "the unaccepted text was checked again", 4_000);
    } finally {
      stop();
    }
  });
});

describe("watchAftConfigFiles registration and retries", () => {
  test("a directory replaced while its watch is being set up is watched again", async () => {
    const dir = tempDir();
    const configDir = join(dir, ".cortexkit");
    const file = join(configDir, "aft.jsonc");
    mkdirSync(configDir);
    writeFileSync(file, "{}");
    const watched: string[] = [];
    let replaced = false;
    const fakeWatch = ((path: string) => {
      watched.push(path);
      return { close: () => {}, on: () => {} };
    }) as unknown as typeof import("node:fs").watch;
    const stop = watchAftConfigFiles({
      paths: [file],
      onChange: () => undefined,
      watchImpl: fakeWatch,
      beforeWatchForTest: (target) => {
        if (replaced || target !== configDir) return;
        replaced = true;
        // Replace the directory between the identity read and the watch.
        rmSync(configDir, { recursive: true, force: true });
        mkdirSync(configDir);
        writeFileSync(file, "{}");
      },
    });
    try {
      await waitUntil(
        () => watched.filter((path) => path === configDir).length >= 2,
        "the replaced directory was watched again",
        4_000,
      );
    } finally {
      stop();
    }
  });

  test("a delete event for the watched directory re-arms its watch", async () => {
    // ext4 reuses a freed inode number at once, so after a delete-and-recreate
    // only the delete event tells that inotify's watch is on a dead directory.
    // The fake watch reports that event while the directory's identity stays
    // the same, which is what such a platform shows.
    const dir = tempDir();
    const configDir = join(dir, ".cortexkit");
    const file = join(configDir, "aft.jsonc");
    mkdirSync(configDir);
    writeFileSync(file, "{}");
    const listeners: Array<(event: string, filename: string | null) => void> = [];
    const watched: string[] = [];
    const fakeWatch = ((
      path: string,
      _options: unknown,
      listener: (event: string, filename: string | null) => void,
    ) => {
      watched.push(path);
      listeners.push(listener);
      return { close: () => {}, on: () => {} };
    }) as unknown as typeof import("node:fs").watch;
    const stop = watchAftConfigFiles({
      paths: [file],
      debounceMs: 20,
      onChange: () => undefined,
      watchImpl: fakeWatch,
    });
    try {
      expect(watched).toEqual([configDir]);
      listeners[0]?.("rename", ".cortexkit");
      await waitUntil(() => watched.length >= 2, "the watch was re-armed", 4_000);
      expect(watched).toEqual([configDir, configDir]);
    } finally {
      stop();
    }
  });

  test("a rejected text is checked again without another event", async () => {
    const dir = tempDir();
    const file = join(dir, "aft.jsonc");
    writeFileSync(file, "{}");
    const results = [false, true];
    let calls = 0;
    const stop = watchAftConfigFiles({
      paths: [file],
      debounceMs: 20,
      onChange: () => results[calls++] ?? true,
    });
    try {
      // Let the watch settle; an event right after it starts can be missed.
      await new Promise((resolve) => setTimeout(resolve, 300));
      writeFileSync(file, '{ "restrict_to_project_root": true }');
      await waitUntil(() => calls >= 2, "the rejected text was retried", 4_000);
    } finally {
      stop();
    }
  });
});

test("fresh filesystem events supersede rejected config retry timers", () => {
  const file = join(tempDir(), "aft.jsonc");
  writeFileSync(file, "{}");
  const pending = new Map<object, () => void>();
  const originalSet = globalThis.setTimeout;
  const originalClear = globalThis.clearTimeout;
  let emit!: () => void;
  let stop: (() => void) | undefined;
  globalThis.setTimeout = ((callback: () => void) => {
    const timer = { unref() {} };
    pending.set(timer, callback);
    return timer;
  }) as unknown as typeof setTimeout;
  globalThis.clearTimeout = ((timer: object) => {
    pending.delete(timer);
  }) as unknown as typeof clearTimeout;
  try {
    stop = watchAftConfigFiles({
      paths: [file],
      checkAtStart: true,
      initialTexts: { [file]: null },
      onChange: () => false,
      watchImpl: ((_path: string, _options: unknown, callback: () => void) => {
        emit = callback;
        return { close() {}, on() {} };
      }) as unknown as typeof import("node:fs").watch,
    });
    expect(pending.size).toBe(1);
    emit();
    expect(pending.size).toBe(1);
    const [timer, check] = pending.entries().next().value!;
    pending.delete(timer);
    check();
    expect(pending.size).toBe(1);
    stop();
    expect(pending.size).toBe(0);
  } finally {
    stop?.();
    globalThis.setTimeout = originalSet;
    globalThis.clearTimeout = originalClear;
  }
});
