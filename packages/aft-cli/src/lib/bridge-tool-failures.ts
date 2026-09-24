import { closeSync, existsSync, openSync, readSync, statSync } from "node:fs";
import { resolvePluginLogPath } from "@cortexkit/aft-bridge";
import { sanitizeContent } from "./sanitize.js";

/** Newest window: tail of the bridge log by bytes (not full file). */
export const BRIDGE_LOG_TAIL_BYTES = 2 * 1024 * 1024;

export const MAX_TOOL_FAILURE_CLASSES = 30;

const SESSION_TAG_PATTERN =
  /\[ses_[^\]\s]+\]|\[[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\]/g;

const STRUCTURED_CODE_PATTERN = /"code"\s*:\s*"([^"]+)"/;

export function resolveBridgePluginLogPath(): string {
  return resolvePluginLogPath();
}

/** Read up to `maxBytes` from the end of a log file (UTF-8). */
export function tailLogFileBytes(path: string, maxBytes: number): string {
  if (!existsSync(path) || maxBytes <= 0) return "";
  let fd: number | null = null;
  try {
    const size = statSync(path).size;
    if (size === 0) return "";
    const readLength = Math.min(size, maxBytes);
    const position = size - readLength;
    fd = openSync(path, "r");
    const buffer = Buffer.allocUnsafe(readLength);
    const bytesRead = readSync(fd, buffer, 0, readLength, position);
    return buffer.subarray(0, bytesRead).toString("utf-8").trim();
  } catch {
    return "";
  } finally {
    if (fd !== null) {
      try {
        closeSync(fd);
      } catch {
        // ignore
      }
    }
  }
}

export type ToolFailureKey = string;

function stripSessionTags(line: string): string {
  return line.replace(SESSION_TAG_PATTERN, "").replace(/\s+/g, " ").trim();
}

/** Remove path-like tokens from aggregated failure labels (belt-and-suspenders). */
function stripPathLikeTokens(label: string): string {
  let out = label;
  out = out.replace(/\/(?:Users|home|var|tmp|private|Volumes)\/[^\s"'`,;)]+/gi, "<path>");
  out = out.replace(/[A-Za-z]:\\(?:Users|Program Files)[^\\s"'`,;)]+/gi, "<path>");
  out = out.replace(/file:\/\/[^\s"'`,;)]+/gi, "<path>");
  out = out.replace(/\(id=[^)]+\)/gi, "");
  return out.replace(/\s+/g, " ").trim();
}

function classifyBridgeLogLine(line: string): ToolFailureKey | null {
  const timeoutMatch = line.match(/Request "([^"]+)"[\s\S]*?timed out after (\d+)ms/i);
  if (timeoutMatch) {
    return `${timeoutMatch[1]}: timed out after ${timeoutMatch[2]}ms`;
  }
  const lowerTimeout = line.match(/request "([^"]+)" timed out after (\d+)ms/i);
  if (lowerTimeout) {
    return `${lowerTimeout[1]}: timed out after ${lowerTimeout[2]}ms`;
  }

  if (/Bridge killed after timeout/i.test(line)) {
    return "bridge killed after timeout";
  }
  if (/bridge killed during sibling timeout/i.test(line)) {
    return "bridge killed during sibling timeout";
  }
  if (/restarting bridge/i.test(line)) {
    return "restarting bridge";
  }

  const rpcMatch = line.match(/RPC error:\s*([^\s=]+)/i);
  if (rpcMatch) {
    return `rpc: RPC error (${rpcMatch[1]})`;
  }

  if (/spawn error/i.test(line)) {
    return "spawn: spawn error";
  }
  if (/failed to spawn/i.test(line)) {
    return "spawn: failed to spawn";
  }

  if (/\bonnx/i.test(line) && /\b(?:error|failed|missing|incompatible)\b/i.test(line)) {
    return "onnx: onnx runtime error";
  }
  if (/\bORT_/i.test(line) && /\b(?:error|failed)\b/i.test(line)) {
    return "onnx: ORT error";
  }

  const codeMatch = line.match(STRUCTURED_CODE_PATTERN);
  if (codeMatch && /\bERROR\b/.test(line)) {
    return `code: ${codeMatch[1]}`;
  }

  if (/\bERROR\b/.test(line) && /\[aft-plugin\]|\[aft-bridge\]|\[aft-lsp\]|\[aft\]/i.test(line)) {
    return "error: ERROR log line";
  }

  return null;
}

/**
 * Aggregate failure-shaped lines from bridge plugin log text into counts.
 * Input should be a recent tail only; keys are tool/command + error class.
 */
export function aggregateBridgeToolFailures(logText: string): Map<ToolFailureKey, number> {
  const counts = new Map<ToolFailureKey, number>();
  if (!logText.trim()) return counts;

  for (const rawLine of logText.split(/\r?\n/)) {
    if (!stripSessionTags(rawLine)) continue;
    const key = classifyBridgeLogLine(rawLine);
    if (!key) continue;
    const sanitizedKey = stripPathLikeTokens(sanitizeContent(key));
    if (!sanitizedKey) continue;
    counts.set(sanitizedKey, (counts.get(sanitizedKey) ?? 0) + 1);
  }

  return counts;
}

function sortFailureKeys(a: string, b: string, counts: Map<string, number>): number {
  const countDiff = (counts.get(b) ?? 0) - (counts.get(a) ?? 0);
  if (countDiff !== 0) return countDiff;
  return a.localeCompare(b);
}

/**
 * Render the `### Recent AFT tool failures` markdown section from aggregated counts.
 */
export function formatRecentAftToolFailuresSection(
  counts: Map<ToolFailureKey, number>,
  options?: { maxClasses?: number },
): string {
  const maxClasses = options?.maxClasses ?? MAX_TOOL_FAILURE_CLASSES;
  const heading = "### Recent AFT tool failures";

  if (counts.size === 0) {
    return `${heading}\nNo recent AFT tool failures recorded.`;
  }

  const sorted = [...counts.keys()].sort((a, b) => sortFailureKeys(a, b, counts));
  const shown = sorted.slice(0, maxClasses);
  const hidden = sorted.length - shown.length;

  const bullets = shown.map((key) => {
    const count = counts.get(key) ?? 0;
    return `- ${key} ×${count}`;
  });
  if (hidden > 0) {
    bullets.push(`- +${hidden} more failure class(es) omitted`);
  }

  return [heading, ...bullets].join("\n");
}

/**
 * Build the tool-failures section from the bridge log path (newest tail window).
 */
export function buildRecentAftToolFailuresSectionFromLog(
  logPath: string = resolveBridgePluginLogPath(),
  options?: { tailBytes?: number; maxClasses?: number },
): string {
  const tailBytes = options?.tailBytes ?? BRIDGE_LOG_TAIL_BYTES;
  const tail = tailLogFileBytes(logPath, tailBytes);
  const counts = aggregateBridgeToolFailures(tail);
  return formatRecentAftToolFailuresSection(counts, options);
}
