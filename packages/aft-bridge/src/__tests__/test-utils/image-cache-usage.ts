import { spawnSync } from "node:child_process";
import { readdirSync, readFileSync, realpathSync, statSync } from "node:fs";
import { basename, join } from "node:path";

export type ImageUse = "found" | "absent" | "unknown";

export interface CacheUsage {
  pidAlive(pid: number): boolean;
  inUse(directory: string): ImageUse;
}

interface ProbeFailure {
  path: string;
  error: unknown;
}

interface ProcessObserver {
  path(path: string): void;
  key(key: string): void;
}

export interface LinuxProbeInputs {
  processPaths: readonly string[];
  mountinfo: string | null;
  probe?: (directory: string, observer: ProcessObserver) => readonly ProbeFailure[];
}

interface UsageSnapshot extends CacheUsage {
  unreadable: readonly { pid: number; paths: readonly string[] }[];
  procfsIncomplete: string | undefined;
}

export function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code !== "ESRCH";
  }
}

function fileKey(path: string): string {
  if (process.platform === "win32") return realpathSync(path).toLowerCase();
  const stat = statSync(path, { bigint: true });
  return `${stat.dev}:${stat.ino}`;
}

export function scanProcessWithRetry(scan: () => void, alive: () => boolean): void {
  for (let attempt = 0; attempt < 3; attempt++) {
    try {
      scan();
      return;
    } catch (error) {
      if (!alive()) return;
      if (attempt === 2) throw error;
      // Visibility in Linux's /proc filesystem can briefly change during exec or process exit.
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 10);
    }
  }
}

function procfsRestriction(mountinfo: string | null): string | undefined {
  if (mountinfo === null) return "cannot read /proc/self/mountinfo";
  let found = false;
  for (const line of mountinfo.split("\n")) {
    const [mount, filesystem] = line.split(" - ");
    const fields = mount?.split(/\s+/);
    const fs = filesystem?.split(/\s+/);
    if (fields?.[4] !== "/proc" || fs?.[0] !== "proc") continue;
    found = true;
    for (const option of `${fields[5]},${fs[2]}`.split(",")) {
      if (option.startsWith("hidepid=")) {
        const value = option.slice("hidepid=".length);
        if (value !== "0" && value !== "off") return `/proc is mounted with hidepid=${value}`;
      }
    }
  }
  return found ? undefined : "cannot identify the /proc mount in mountinfo";
}

function probeLinuxProcess(directory: string, observer: ProcessObserver): ProbeFailure[] {
  const pid = Number(basename(directory));
  try {
    if (statSync(directory).uid !== process.geteuid?.()) return [];
  } catch (error) {
    return pidAlive(pid) ? [{ path: directory, error }] : [];
  }
  let failures: ProbeFailure[] = [];
  const record = (path: string) => {
    try {
      observer.path(path);
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") failures.push({ path, error });
    }
  };
  try {
    scanProcessWithRetry(
      () => {
        failures = [];
        record(join(directory, "exe"));
        const fdPath = join(directory, "fd");
        try {
          for (const fd of readdirSync(fdPath)) record(join(fdPath, fd));
        } catch (error) {
          failures.push({ path: fdPath, error });
        }
        const mapsPath = join(directory, "maps");
        let maps = "";
        try {
          maps = readFileSync(mapsPath, "utf8");
        } catch (error) {
          failures.push({ path: mapsPath, error });
        }
        for (const line of maps.split("\n")) {
          const match = /^\S+\s+\S+\s+\S+\s+([a-f\d]+):([a-f\d]+)\s+(\d+)/i.exec(line);
          if (!match || match[3] === "0") continue;
          const major = BigInt(`0x${match[1]}`);
          const minor = BigInt(`0x${match[2]}`);
          const device =
            ((major & 0xfffn) << 8n) |
            (minor & 0xffn) |
            ((minor & ~0xffn) << 12n) |
            ((major & ~0xfffn) << 32n);
          observer.key(`${device}:${match[3]}`);
        }
        const failure = failures[0];
        if (failure) throw failure.error;
      },
      () => {
        if (!pidAlive(pid)) return false;
        try {
          const stat = readFileSync(join(directory, "stat"), "utf8");
          const state = stat.slice(stat.lastIndexOf(") ") + 2)[0];
          return state !== "Z" && state !== "X";
        } catch (error) {
          return (error as NodeJS.ErrnoException).code !== "ENOENT";
        }
      },
    );
    return [];
  } catch (error) {
    return failures.length > 0 ? failures : [{ path: directory, error }];
  }
}

/** Supplied Linux inputs let tests avoid scanning unrelated host processes. */
export function cacheUsageSnapshot(linux?: LinuxProbeInputs): UsageSnapshot {
  const files = new Set<string>();
  const observer: ProcessObserver = {
    path: (path) => {
      files.add(fileKey(path));
    },
    key: (key) => {
      files.add(key);
    },
  };
  const unreadable: { pid: number; paths: string[] }[] = [];
  let procfsIncomplete: string | undefined;
  if (linux || process.platform === "linux") {
    let mountinfo: string | null;
    if (linux) mountinfo = linux.mountinfo;
    else {
      try {
        mountinfo = readFileSync("/proc/self/mountinfo", "utf8");
      } catch {
        mountinfo = null;
      }
    }
    procfsIncomplete = procfsRestriction(mountinfo);
    const directories =
      linux?.processPaths ??
      readdirSync("/proc")
        .filter((entry) => /^\d+$/.test(entry))
        .map((entry) => join("/proc", entry));
    const probe = linux?.probe ?? probeLinuxProcess;
    for (const directory of directories) {
      const name = basename(directory);
      if (!/^\d+$/.test(name)) continue;
      let failures: readonly ProbeFailure[];
      try {
        failures = probe(directory, observer);
      } catch (error) {
        failures = [{ path: directory, error }];
      }
      if (failures.length > 0) {
        unreadable.push({
          pid: Number(name),
          paths: [...new Set(failures.map((failure) => failure.path))].sort(),
        });
      }
    }
  } else {
    // lsof includes open descriptors and executable mappings, including aliases
    // of an image's inode. Windows also refuses to move executing .exe files.
    const result =
      process.platform === "darwin"
        ? spawnSync("/usr/sbin/lsof", ["-nP", "-Fpn", "-a", "-u", String(process.geteuid?.())], {
            encoding: "utf8",
            maxBuffer: 64 * 1024 ** 2,
          })
        : spawnSync(
            "powershell.exe",
            [
              "-NoProfile",
              "-NonInteractive",
              "-Command",
              "Get-CimInstance Win32_Process | ForEach-Object { $_.ExecutablePath }",
            ],
            { encoding: "utf8", maxBuffer: 64 * 1024 ** 2 },
          );
    if (result.error) throw result.error;
    if (result.status !== 0) throw new Error("cannot enumerate live fixture images");
    for (const line of result.stdout.split(/\r?\n/)) {
      const path =
        process.platform === "darwin" ? (line.startsWith("n") ? line.slice(1) : "") : line;
      if (!path) continue;
      try {
        observer.path(path);
      } catch {
        // Descriptors may close while the process table is being sampled.
      }
    }
  }
  const containsFile = (directory: string): boolean =>
    readdirSync(directory, { withFileTypes: true }).some((entry) => {
      const path = join(directory, entry.name);
      return (
        (entry.isFile() && files.has(fileKey(path))) || (entry.isDirectory() && containsFile(path))
      );
    });
  return {
    pidAlive,
    unreadable,
    procfsIncomplete,
    inUse: (directory) =>
      containsFile(directory)
        ? "found"
        : unreadable.length > 0 || procfsIncomplete !== undefined
          ? "unknown"
          : "absent",
  };
}
