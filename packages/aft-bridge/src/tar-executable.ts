import { existsSync } from "node:fs";
import { join } from "node:path";
import { execFileSync } from "./child-process.js";

interface TarResolverOptions {
  platform?: NodeJS.Platform;
  env?: NodeJS.ProcessEnv;
  exists?: (path: string) => boolean;
}

/**
 * Resolve the archive extractor without allowing MSYS2 or Cygwin to shadow
 * Windows' drive-path-aware bsdtar executable.
 */
export function windowsTarExecutable(options: TarResolverOptions = {}): string {
  const platform = options.platform ?? process.platform;
  if (platform !== "win32") return "tar";

  const env = options.env ?? process.env;
  const systemTar = join(env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe");
  return (options.exists ?? existsSync)(systemTar) ? systemTar : "tar";
}

/** Run an extraction command and preserve the resolved executable in failures. */
export function execTarExtractionSync(args: string[], timeout: number): void {
  const executable = windowsTarExecutable();
  try {
    execFileSync(executable, args, { stdio: "pipe", timeout });
  } catch (cause) {
    const detail = cause instanceof Error ? cause.message : String(cause);
    throw new Error(`tar extraction failed using ${executable}: ${detail}`, { cause });
  }
}
