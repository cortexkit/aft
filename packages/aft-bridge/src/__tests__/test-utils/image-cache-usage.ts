import { spawnSync } from "node:child_process";
import { readdirSync, readFileSync, realpathSync, statSync } from "node:fs";
import { join } from "node:path";

export interface CacheUsage {
  pidAlive(pid: number): boolean;
  inUse(directory: string): boolean;
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

export function cacheUsageSnapshot(): CacheUsage {
  const files = new Set<string>();
  const record = (path: string) => {
    try {
      files.add(fileKey(path));
    } catch {
      // Descriptors may close while the process table is being sampled.
    }
  };
  if (process.platform === "linux") {
    for (const entry of readdirSync("/proc")) {
      if (!/^\d+$/.test(entry)) continue;
      const directory = join("/proc", entry);
      try {
        if (statSync(directory).uid !== process.geteuid?.()) continue;
        record(join(directory, "exe"));
        for (const fd of readdirSync(join(directory, "fd"))) record(join(directory, "fd", fd));
        for (const line of readFileSync(join(directory, "maps"), "utf8").split("\n")) {
          const match = /^\S+\s+\S+\s+\S+\s+([a-f\d]+):([a-f\d]+)\s+(\d+)/i.exec(line);
          if (!match || match[3] === "0") continue;
          const major = BigInt(`0x${match[1]}`);
          const minor = BigInt(`0x${match[2]}`);
          const device =
            ((major & 0xfffn) << 8n) |
            (minor & 0xffn) |
            ((minor & ~0xffn) << 12n) |
            ((major & ~0xfffn) << 32n);
          files.add(`${device}:${match[3]}`);
        }
      } catch (error) {
        if (pidAlive(Number(entry))) throw error;
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
      if (process.platform === "darwin") {
        if (line.startsWith("n")) record(line.slice(1));
      } else if (line) record(line);
    }
  }
  const inUse = (directory: string): boolean =>
    readdirSync(directory, { withFileTypes: true }).some((entry) => {
      const path = join(directory, entry.name);
      return (entry.isFile() && files.has(fileKey(path))) || (entry.isDirectory() && inUse(path));
    });
  return { pidAlive, inUse };
}
