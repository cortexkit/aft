import { type ToolDefinition, tool } from "@opencode-ai/plugin";

export type SharedBehaviorCase = {
  name: string;
  input: Record<string, unknown>;
  definition: ToolDefinition;
  v1Output: unknown;
  v2Output: unknown;
};

export const SHARED_BEHAVIOR_CASES: readonly SharedBehaviorCase[] = [
  {
    name: "aft_echo",
    input: { value: "shared-case" },
    definition: {
      description: "Return the supplied fixture value",
      args: { value: tool.schema.string() },
      execute: async (input, context) => ({
        title: "Echo",
        output: `${String(input.value)}@${context.worktree}`,
        metadata: { sessionID: context.sessionID },
      }),
    },
    v1Output: {
      title: "Echo",
      output: "shared-case@/work/canonical",
      metadata: { sessionID: "behavior-v1" },
    },
    v2Output: {
      content: "shared-case@/work/canonical",
      metadata: { sessionID: "behavior-v2", title: "Echo" },
    },
  },
];
