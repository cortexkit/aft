interface TuiEntry {
  readonly id: string;
  readonly tui: (api: unknown, options?: unknown, meta?: unknown) => Promise<void>;
  readonly setup: (context: unknown) => Promise<void>;
}

declare const entry: TuiEntry;
export default entry;
