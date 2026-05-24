import { execSync, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import { homedir } from "node:os";
import { join } from "node:path";
import { getAftBinaryCacheDir, getAftBinaryName } from "./paths.js";

function normalizeVersion(output: string): string | null {
  const trimmed = output.trim();
  if (!trimmed) return null;
  return trimmed.replace(/^aft\s+/, "");
}

/**
 * Probe `aft --version` from the same prioritized candidate locations used by
 * `findAftBinary()` (cache, npm platform package, PATH, cargo fallback).
 *
 * Returns the first successfully reported version, or null if nothing
 * resolves. Errors and missing files are swallowed — callers get a signal,
 * not an exception.
 */
export function probeBinaryVersion(preferredVersion?: string): string | null {
  const candidate = findAftBinary(preferredVersion);
  if (!candidate) return null;

  try {
    if (!existsSync(candidate)) return null;
    const result = spawnSync(candidate, ["--version"], {
      stdio: ["ignore", "pipe", "pipe"],
      encoding: "utf-8",
    });
    if (result.error || result.status !== 0) return null;
    const output = `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
    return normalizeVersion(output);
  } catch {
    return null;
  }
}

function pushCandidate(candidates: string[], candidate: string | null | undefined): void {
  if (!candidate) return;
  if (!candidates.includes(candidate)) candidates.push(candidate);
}

function firstExisting(candidates: string[]): string | null {
  for (const candidate of candidates) {
    try {
      if (!existsSync(candidate)) continue;
      return candidate;
    } catch {
      // try next
    }
  }
  return null;
}

export function platformKey(
  platform: string = process.platform,
  arch: string = process.arch,
): string | null {
  const table: Record<string, Record<string, string>> = {
    darwin: { arm64: "darwin-arm64", x64: "darwin-x64" },
    linux: { arm64: "linux-arm64", x64: "linux-x64" },
    win32: { x64: "win32-x64" },
  };
  return table[platform]?.[arch] ?? null;
}

export function findAftBinary(preferredVersion?: string): string | null {
  const candidates: string[] = [];
  if (preferredVersion) {
    const tag = preferredVersion.startsWith("v") ? preferredVersion : `v${preferredVersion}`;
    pushCandidate(candidates, join(getAftBinaryCacheDir(), tag, getAftBinaryName()));
  }

  const key = platformKey();
  if (key) {
    try {
      const require = createRequire(import.meta.url);
      pushCandidate(candidates, require.resolve(`@cortexkit/aft-${key}/bin/${getAftBinaryName()}`));
    } catch {
      // platform package is optional
    }
  }

  try {
    const lookup = process.platform === "win32" ? "where aft" : "which aft";
    const resolved = execSync(lookup, { stdio: "pipe", encoding: "utf-8" }).trim();
    if (resolved) {
      pushCandidate(candidates, resolved.split(/\r?\n/)[0]);
    }
  } catch {
    // ignore — PATH lookup is best-effort
  }

  pushCandidate(candidates, join(homedir(), ".cargo", "bin", getAftBinaryName()));

  return firstExisting(candidates);
}
