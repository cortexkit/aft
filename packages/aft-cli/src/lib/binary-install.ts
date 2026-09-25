import { CLI } from "./cli.js";
import { captureBridgeLog } from "./cli-logger.js";
import { formatFsError, isPermissionError } from "./fs-errors.js";

/**
 * Obtain the native binary matching this CLI. Setup and `doctor --fix` both
 * come through here, so a clean machine gets the same download (and the same
 * failure explanation) from either command.
 */

/** Downloads the binary for a `v`-prefixed tag; resolves to its path or null. */
export type BinaryDownloader = (tag: string) => Promise<string | null>;

export type BinaryFailureCause =
  | "permission"
  | "network"
  | "missing_asset"
  | "checksum"
  | "unsupported_platform"
  | "other";

export type BinaryObtainResult =
  | { ok: true; path: string }
  | { ok: false; cause: BinaryFailureCause; message: string };

async function defaultDownloader(tag: string): Promise<string | null> {
  // Literal specifier so the bundle inlines aft-bridge instead of resolving
  // the installed package at runtime (whose dist chain loads subc-client's
  // TypeScript entry, which Node cannot load).
  const { ensureBinary } = await import("@cortexkit/aft-bridge");
  return ensureBinary(tag);
}

const NETWORK =
  /fetch failed|ENOTFOUND|EAI_AGAIN|ECONNREFUSED|ECONNRESET|ETIMEDOUT|ENETUNREACH|EHOSTUNREACH|socket hang up|network|aborted|timed? ?out|HTTP 5\d\d|HTTP 403|HTTP 429/i;

/**
 * Classify why a download failed from the error it threw or the lines it
 * logged. The downloader reports its cause only through the log, so every
 * line is considered; the first recognised cause wins, in order of how
 * specific it is.
 */
export function classifyBinaryFailure(
  tag: string,
  evidence: string[],
): { cause: BinaryFailureCause; message: string } {
  const text = evidence.join("\n");
  const permissionLine = evidence.find((line) => isPermissionError(line));
  if (permissionLine) {
    return { cause: "permission", message: formatFsError(new Error(permissionLine)) };
  }
  if (/Unsupported platform/i.test(text)) {
    return {
      cause: "unsupported_platform",
      message: `no AFT binary is published for ${process.platform}-${process.arch}.`,
    };
  }
  if (/HTTP 404/.test(text)) {
    return {
      cause: "missing_asset",
      message: `the ${tag} release has no binary for ${process.platform}-${process.arch} on GitHub (HTTP 404). The release may still be publishing; try again in a few minutes.`,
    };
  }
  if (/checksum/i.test(text)) {
    const detail = evidence.find((line) => /checksum/i.test(line)) ?? "";
    return {
      cause: "checksum",
      message: `the downloaded binary failed checksum verification and was discarded (${detail.trim()}).`,
    };
  }
  if (NETWORK.test(text)) {
    const detail = evidence.find((line) => NETWORK.test(line)) ?? "";
    return {
      cause: "network",
      message: `could not reach GitHub to download the ${tag} binary (${detail.trim()}). Check your network connection or proxy.`,
    };
  }
  const last = evidence.filter((line) => line.trim().length > 0).at(-1);
  return {
    cause: "other",
    message: last ? `the ${tag} download failed: ${last.trim()}` : `the ${tag} download failed.`,
  };
}

/** Download the binary for `version`, reporting the real cause on failure. */
export async function obtainAftBinary(
  version: string,
  download: BinaryDownloader = defaultDownloader,
): Promise<BinaryObtainResult> {
  const tag = version.startsWith("v") ? version : `v${version}`;
  let thrown: unknown = null;
  const { result, lines } = await captureBridgeLog(async () => {
    try {
      return await download(tag);
    } catch (error) {
      thrown = error;
      return null;
    }
  });
  if (result) return { ok: true, path: result };

  const evidence = lines.filter((line) => line.level !== "info").map((line) => line.message);
  if (thrown !== null) {
    if (isPermissionError(thrown)) {
      return { ok: false, cause: "permission", message: formatFsError(thrown) };
    }
    evidence.push(thrown instanceof Error ? thrown.message : String(thrown));
  }
  return { ok: false, ...classifyBinaryFailure(tag, evidence) };
}

/** The command a user runs to retry the binary install from another command. */
export const BINARY_RETRY_COMMAND = `${CLI} doctor --fix`;
