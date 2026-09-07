import type { Plugin as V1Plugin } from "@opencode-ai/plugin";

interface ServerEntry {
  readonly id: string;
  readonly server: V1Plugin;
  readonly effect: (context: unknown) => unknown;
}

declare const entry: ServerEntry;
export default entry;
