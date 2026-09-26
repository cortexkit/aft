import type { AftProjectTransport, AftTransportPool } from "@cortexkit/aft-bridge";
import type { AftConfig } from "./config.js";
import { resolveBashConfig } from "./config.js";
import { debug, log, warn } from "./logger.js";
import { BASH_TRANSPORT_TIMEOUT_MS } from "./tools/_shared.js";

type ActiveBridgePool = Pick<AftTransportPool, "getActiveBridgeForRoot" | "activeBridges">;

export const BASH_WAIT_DETACH_MAGIC_KEYWORD = "&detach";
const EMPTY_DETACH_MESSAGE = "(requested background detach)";
const STANDALONE_DETACH_KEYWORD_SOURCE = `(^|[^\\p{L}\\p{N}_])${BASH_WAIT_DETACH_MAGIC_KEYWORD}(?![\\p{L}\\p{N}_])`;
const STANDALONE_DETACH_KEYWORD_PATTERN = new RegExp(STANDALONE_DETACH_KEYWORD_SOURCE, "u");
const STANDALONE_DETACH_KEYWORDS_PATTERN = new RegExp(STANDALONE_DETACH_KEYWORD_SOURCE, "gu");

function containsStandaloneDetachKeyword(messageText: string): boolean {
  return STANDALONE_DETACH_KEYWORD_PATTERN.test(messageText);
}

function stripStandaloneDetachKeywords(messageText: string): string {
  return messageText.replace(STANDALONE_DETACH_KEYWORDS_PATTERN, "$1");
}

type UserTextPart = { type?: unknown; text?: unknown; synthetic?: unknown; ignored?: unknown };

function userTextParts(output: unknown): Array<UserTextPart & { text: string }> {
  if (!output || typeof output !== "object") return [];
  const parts = (output as { parts?: unknown }).parts;
  if (!Array.isArray(parts)) return [];
  return parts.filter((part): part is UserTextPart & { text: string } => {
    if (!part || typeof part !== "object") return false;
    const textPart = part as UserTextPart;
    return (
      textPart.type === "text" &&
      typeof textPart.text === "string" &&
      textPart.synthetic !== true &&
      textPart.ignored !== true
    );
  });
}

/** Return the user-entered text parts from OpenCode's chat.message output. */
export function extractUserMessageText(output: unknown): string {
  return userTextParts(output)
    .map((part) => part.text)
    .join("\n");
}

/** Strip control tokens from mutable OpenCode text parts before model delivery. */
export function stripUserMessageDetachKeyword(output: unknown): string {
  const parts = userTextParts(output);
  const original = parts.map((part) => part.text).join("\n");
  if (!containsStandaloneDetachKeyword(original)) return original;

  const stripped = parts
    .map((part) => stripStandaloneDetachKeywords(part.text))
    .map((text) => text.replace(/[ \t]{2,}/g, " "));
  if (stripped.join("\n").trim() === "") {
    if (parts[0]) parts[0].text = EMPTY_DETACH_MESSAGE;
    for (const part of parts.slice(1)) part.text = "";
    return EMPTY_DETACH_MESSAGE;
  }

  parts.forEach((part, index) => {
    part.text = stripped[index];
  });
  return stripped.join("\n");
}

/**
 * Decide whether a user message should signal an active wait:true bash call.
 * The keyword is checked before the host hook strips it from mutable text parts.
 */
export function shouldDetachBashWaitOnUserMessage(config: AftConfig, messageText: string): boolean {
  return (
    resolveBashConfig(config).detach_on_user_message || containsStandaloneDetachKeyword(messageText)
  );
}

async function sendBashWaitDetach(
  bridge: AftProjectTransport,
  sessionID: string,
): Promise<boolean> {
  const response = await bridge.send(
    "bash_wait_detach",
    { session_id: sessionID },
    { keepBridgeOnTimeout: true, transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS },
  );
  if (response.success === false) {
    throw new Error(String(response.message ?? "bash_wait_detach failed"));
  }
  // success:true with detached:false means no active wait was found under
  // this session on that bridge — possibly the wrong bridge. Callers use
  // this to keep fanning out instead of treating delivery as done.
  return response.detached === true;
}

export async function signalBashWaitDetachForProject(
  pool: ActiveBridgePool,
  projectRoot: string,
  sessionID: string | undefined,
): Promise<void> {
  if (!sessionID) return;
  // Try the exact root first, but keep fanning out while no bridge reports an
  // actually-detached wait: success:true + detached:false from the exact-root
  // bridge means the wait lives elsewhere (root-key mismatch), not "done".
  const exact = pool.getActiveBridgeForRoot(projectRoot);
  const all = pool.activeBridges();
  const targets = exact ? [exact, ...all.filter((bridge) => bridge !== exact)] : all;
  if (targets.length === 0) {
    // A wait lives inside a bridge, so with no bridge there is nothing to
    // detach. This is the normal state before the first tool call of a
    // session starts one, not a fault, so it is not logged as a warning.
    debug(
      `[bash_wait_detach] no bridge running yet for session ${sessionID} (root ${projectRoot}); nothing to detach`,
    );
    return;
  }
  let signaled = 0;
  let lastError: unknown = null;
  for (const bridge of targets) {
    try {
      signaled += 1;
      if (await sendBashWaitDetach(bridge, sessionID)) {
        log(
          `[bash_wait_detach] detached wait for session ${sessionID} (bridge ${signaled}/${targets.length})`,
        );
        return;
      }
    } catch (err) {
      lastError = err;
    }
  }
  if (lastError !== null) {
    warn(
      `[bash_wait_detach] failed for session ${sessionID}: ${lastError instanceof Error ? lastError.message : String(lastError)}`,
    );
  } else {
    log(
      `[bash_wait_detach] no active wait found for session ${sessionID} (signaled ${signaled} bridge(s))`,
    );
  }
}
