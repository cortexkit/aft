import { capSearchContext } from "@cortexkit/aft-bridge";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { AftConfig } from "../config.js";

export function collectPiSearchContext(
  config: AftConfig,
  extCtx: ExtensionContext,
  toolCallId: string,
): string | undefined {
  if (config.search?.gather?.enabled !== true) return undefined;
  try {
    const manager = extCtx.sessionManager;
    if (typeof manager?.getLeafEntry !== "function" || typeof manager.getBranch !== "function") {
      return undefined;
    }
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

    const branch = manager.getBranch(leaf.id);
    if (!Array.isArray(branch) || branch.at(-1)?.id !== leaf.id) return undefined;
    const text: string[] = [];
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
    for (const part of content.slice(0, toolIndex)) {
      if (part.type === "thinking") text.push(part.thinking);
      else if (part.type === "text") text.push(part.text);
    }
    const trace = text.filter((part) => part.length > 0).join("\n");
    return trace.length > 0 ? capSearchContext(trace) : undefined;
  } catch {
    return undefined;
  }
}
