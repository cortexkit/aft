import { capSearchContext } from "@cortexkit/aft-bridge";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import { type AftConfig, gatherEnabled } from "../config.js";
import { warn } from "../logger.js";

export function collectPiSearchContext(
  config: AftConfig,
  extCtx: ExtensionContext,
  toolCallId: string,
): string | undefined {
  if (!gatherEnabled(config)) return undefined;
  try {
    const manager = extCtx.sessionManager;
    if (typeof manager?.getLeafEntry !== "function" || typeof manager.getBranch !== "function") {
      return undefined;
    }
    // An older assistant can contain unrelated text when the current turn is not stored.
    const leaf = manager.getLeafEntry();
    if (
      leaf?.type !== "message" ||
      typeof leaf.id !== "string" ||
      leaf.id.length === 0 ||
      leaf.message.role !== "assistant" ||
      !Array.isArray(leaf.message.content)
    ) {
      return undefined;
    }
    const content = leaf.message.content;
    const toolIndex = content.findIndex(
      (part) => part.type === "toolCall" && part.id === toolCallId,
    );
    if (toolIndex < 0) return undefined;

    // The active branch can change. Its end must match the captured leaf.
    const branch = manager.getBranch(leaf.id);
    if (!Array.isArray(branch) || branch.at(-1)?.id !== leaf.id) return undefined;
    const text: string[] = [];
    for (const part of content.slice(0, toolIndex)) {
      if (part.type === "thinking") text.push(part.thinking);
      else if (part.type === "text") text.push(part.text);
    }
    // The shared limit keeps the end. User text goes last so long reasoning cannot remove it.
    for (let index = branch.length - 2; index >= 0; index--) {
      const entry = branch[index];
      if (entry.type !== "message" || entry.message.role !== "user") continue;
      const userContent = entry.message.content;
      if (typeof userContent === "string") text.push(userContent);
      else {
        for (const part of userContent) {
          if (part.type === "text") text.push(part.text);
        }
      }
      break;
    }
    const trace = text.filter((part) => part.trim().length > 0).join("\n\n");
    return trace.length > 0 ? capSearchContext(trace) : undefined;
  } catch (error) {
    warn(`gather context unavailable: ${error instanceof Error ? "Error" : typeof error}`);
    return undefined;
  }
}
