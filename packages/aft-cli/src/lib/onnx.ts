import { existsSync, readdirSync, readlinkSync, realpathSync, statSync } from "node:fs";
import { join, resolve } from "node:path";

export const ONNX_RUNTIME_VERSION = "1.24.4";

export function getOnnxLibraryName(): string {
  if (process.platform === "darwin") return "libonnxruntime.dylib";
  if (process.platform === "win32") return "onnxruntime.dll";
  return "libonnxruntime.so";
}

export function getManualInstallHint(): string {
  const p = process.platform;
  const a = process.arch;
  if (p === "darwin") {
    if (a === "arm64") return "brew install onnxruntime (Apple Silicon)";
    return "Intel Mac requires manual install — see docs";
  }
  if (p === "linux") {
    if (a === "x64" || a === "arm64") {
      return "AFT auto-downloads ONNX Runtime on supported Linux (glibc)";
    }
    return "manual install required for this Linux arch";
  }
  if (p === "win32") {
    if (a === "x64" || a === "arm64") return "AFT auto-downloads ONNX Runtime on Windows";
    return "manual install required for this Windows arch";
  }
  return "ONNX Runtime must be installed manually for this platform";
}

export function findSystemOnnxRuntime(): string | null {
  const libName = getOnnxLibraryName();
  const searchPaths: string[] = [];

  if (process.platform === "darwin") {
    searchPaths.push("/opt/homebrew/lib", "/usr/local/lib");
  } else if (process.platform === "linux") {
    searchPaths.push(
      "/usr/lib",
      "/usr/lib/x86_64-linux-gnu",
      "/usr/lib/aarch64-linux-gnu",
      "/usr/local/lib",
    );
  } else if (process.platform === "win32") {
    const programFiles = process.env.ProgramFiles ?? "C:\\Program Files";
    const programFilesX86 = process.env["ProgramFiles(x86)"] ?? "C:\\Program Files (x86)";
    searchPaths.push(
      join(programFiles, "onnxruntime", "lib"),
      join(programFiles, "Microsoft ONNX Runtime", "lib"),
      join(programFiles, "Microsoft Machine Learning", "lib"),
      join(programFilesX86, "onnxruntime", "lib"),
    );
    // Also search PATH for onnxruntime.dll
    const pathEnv = process.env.PATH ?? "";
    for (const dir of pathEnv.split(";")) {
      const trimmed = dir.trim();
      if (trimmed) searchPaths.push(trimmed);
    }
  }

  // Deduplicate paths.
  // On case-insensitive filesystems (Windows, macOS) normalize casing for
  // comparison; on Linux the raw path casing is the authority.
  const normalizeCase = process.platform === "win32" || process.platform === "darwin";
  const seen = new Set<string>();
  for (const dir of searchPaths) {
    let key = resolve(dir).replace(/[/\\]+$/, "");
    if (normalizeCase) key = key.toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    if (existsSync(join(dir, libName))) return dir;
  }
  return null;
}

export function findCachedOnnxRuntime(storageDir: string): string | null {
  const ortDir = join(storageDir, "onnxruntime", ONNX_RUNTIME_VERSION);
  return existsSync(join(ortDir, getOnnxLibraryName())) ? ortDir : null;
}

/**
 * Detect an installed ONNX Runtime's advertised version by walking the
 * shared-library filename suffixes that Microsoft ships. Returns null when
 * the version can't be determined.
 */
export function detectOrtVersion(libDir: string): string | null {
  if (!existsSync(libDir)) return null;

  // Match both libonnxruntime.so.1.24.4 and symlinks pointing at it.
  const libName = getOnnxLibraryName();
  try {
    const entries = readdirSync(libDir);
    for (const entry of entries) {
      if (!entry.startsWith(libName)) continue;
      const match = entry.match(/\.(\d+\.\d+\.\d+)$/);
      if (match) return match[1];
    }

    // Fall back: libonnxruntime.so or .dylib → follow symlink.
    const base = join(libDir, libName);
    if (existsSync(base)) {
      try {
        const real = realpathSync(base);
        const suffix = real.match(/\.(\d+\.\d+\.\d+)$/);
        if (suffix) return suffix[1];
      } catch {
        // ignore
      }
      try {
        const target = readlinkSync(base);
        const suffix = target.match(/\.(\d+\.\d+\.\d+)$/);
        if (suffix) return suffix[1];
      } catch {
        // not a symlink
      }
    }
  } catch {
    // ignore
  }
  return null;
}

/** Minimum major.minor required by AFT's bundled ort crate. */
export const REQUIRED_ORT_MAJOR = 1;
export const REQUIRED_ORT_MIN_MINOR = 20;

export function isOrtVersionCompatible(version: string): boolean {
  const parts = version.split(".").map((p) => parseInt(p, 10));
  const [major, minor] = parts;
  if (!Number.isFinite(major) || !Number.isFinite(minor)) return false;
  if (major !== REQUIRED_ORT_MAJOR) return false;
  return minor >= REQUIRED_ORT_MIN_MINOR;
}

/** File-stat helper so callers can report age/size of the ONNX dir. */
export function inspectPathStats(path: string): {
  exists: boolean;
  isDir: boolean;
  isFile: boolean;
} {
  if (!existsSync(path)) return { exists: false, isDir: false, isFile: false };
  try {
    const st = statSync(path);
    return { exists: true, isDir: st.isDirectory(), isFile: st.isFile() };
  } catch {
    return { exists: false, isDir: false, isFile: false };
  }
}
