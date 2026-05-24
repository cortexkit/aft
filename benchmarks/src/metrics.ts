import { $ } from "bun";

export interface SessionMetrics {
  sessionId: string;
  tokens: {
    input: number;
    output: number;
    cacheRead: number;
    cacheWrite: number;
    reasoning: number;
    total: number;
  };
  toolCalls: number;
  failedToolCalls: number;
  failedToolCallsByName: Record<string, number>;
  toolCallsByName: Record<string, number>;
  messageCount: number;
  agentTimeMs: number;
}

/**
 * Extract metrics from an opencode session using `opencode export`.
 */
export async function extractMetrics(
  sessionId: string,
): Promise<SessionMetrics> {
  const result =
    await $`opencode export ${sessionId} 2>/dev/null`.text();
  const session = JSON.parse(result);

  let totalInput = 0;
  let totalOutput = 0;
  let totalCacheRead = 0;
  let totalCacheWrite = 0;
  let totalReasoning = 0;
  let totalTokens = 0;
  let toolCalls = 0;
  let failedToolCalls = 0;
  const toolCallsByName: Record<string, number> = {};
  const failedToolCallsByName: Record<string, number> = {};
  let messageCount = 0;
  let firstMessageTime = Infinity;
  let lastMessageTime = 0;

  for (const msg of session.messages ?? []) {
    messageCount++;
    const info = msg.info ?? {};

    // Track time range
    const created = info.time?.created ?? 0;
    if (created < firstMessageTime) firstMessageTime = created;
    if (created > lastMessageTime) lastMessageTime = created;

    // tokens.total is per-API-call (prompt + completion), not cumulative.
    // The last message's total represents the final conversation size.
    // input/output/reasoning are new tokens per call, so we sum those.
    if (info.tokens) {
      totalInput += info.tokens.input ?? 0;
      totalOutput += info.tokens.output ?? 0;
      totalReasoning += info.tokens.reasoning ?? 0;
      totalTokens = info.tokens.total ?? totalTokens;
      totalCacheRead += info.tokens.cache?.read ?? 0;
      totalCacheWrite += info.tokens.cache?.write ?? 0;
    }

    // Count tool calls from parts
    for (const part of msg.parts ?? []) {
      if (
        part.type === "tool-invocation" ||
        part.type === "tool-call" ||
        part.state?.status === "completed"
      ) {
        const toolName = part.tool ?? part.toolName ?? part.state?.tool ?? "unknown";
        if (toolName && toolName !== "unknown") {
          toolCalls++;
          toolCallsByName[toolName] = (toolCallsByName[toolName] ?? 0) + 1;

          // Detect failed tool calls — parse JSON to check "ok" field directly,
          // not substring matching (which false-positives on file content containing "not_found")
          const output = String(part.state?.output ?? "");
          let isFailed = false;
          try {
            const parsed = JSON.parse(output);
            isFailed = parsed.success === false || parsed.ok === false;
          } catch {
            // Non-JSON output (e.g., from bash) — check for error indicators
            isFailed = output.startsWith("Error:") || output.startsWith("error:");
          }
          if (isFailed) {
            failedToolCalls++;
            failedToolCallsByName[toolName] = (failedToolCallsByName[toolName] ?? 0) + 1;
          }
        }
      }
    }
  }

  const agentTimeMs =
    firstMessageTime < Infinity ? lastMessageTime - firstMessageTime : 0;

  return {
    sessionId,
    tokens: {
      input: totalInput,
      output: totalOutput,
      cacheRead: totalCacheRead,
      cacheWrite: totalCacheWrite,
      reasoning: totalReasoning,
      total: totalTokens,
    },
    toolCalls,
    failedToolCalls,
    failedToolCallsByName,
    toolCallsByName,
    messageCount,
    agentTimeMs,
  };
}

/**
 * Parse session ID from opencode run --format json output.
 * JSON mode outputs newline-delimited JSON events. Look for session info.
 */
export function parseSessionId(output: string): string | null {
  // Try parsing each line as JSON
  for (const line of output.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    try {
      const event = JSON.parse(trimmed);
      // Look for session ID in various event formats
      if (event.sessionID) return event.sessionID;
      if (event.session?.id) return event.session.id;
      if (event.type === "session" && event.id) return event.id;
    } catch {
      // Not JSON, try regex fallback
      const match = trimmed.match(/ses_[a-zA-Z0-9]+/);
      if (match) return match[0];
    }
  }

  // Regex fallback on full output
  const match = output.match(/ses_[a-zA-Z0-9]+/);
  return match ? match[0] : null;
}
