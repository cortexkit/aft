import type { BashAckTarget } from "./bash-pattern-ack.js";
import type { BashWatchLineBatch } from "./bridge.js";

type Stream = "stdout" | "stderr" | "pty" | "output";

function knownStream(stream: string): stream is Stream {
  return stream === "stdout" || stream === "stderr" || stream === "pty" || stream === "output";
}

/** Admission watermarks for one pending watch generation; no line text is retained. */
export class WatchBatchProgress {
  private pendingID: string | undefined;
  private version: number | undefined;
  private ends: Record<Stream, number> = { stdout: 0, stderr: 0, pty: 0, output: 0 };
  private dropped = 0;
  private skipped = 0;
  private gapNotices = 0;
  private gapOccurrences = 0;

  get admittedVersion(): number | undefined {
    return this.version;
  }

  unseen(batch: BashWatchLineBatch): BashWatchLineBatch {
    const same = this.pendingID === (batch.pending_id ?? "legacy");
    const obsolete =
      same &&
      !batch.snapshot &&
      batch.version !== undefined &&
      this.version !== undefined &&
      batch.version <= this.version;
    const lines: BashWatchLineBatch["lines"] = [];
    if (!obsolete)
      for (const line of batch.lines) {
        const width = Math.max(1, line.end_offset - line.offset);
        const end = same && knownStream(line.stream) ? this.ends[line.stream] : 0;
        const skip = Math.min(line.count, Math.max(0, Math.ceil((end - line.offset) / width)));
        if (skip < line.count)
          lines.push({
            ...line,
            offset: line.offset + skip * width,
            end_offset: line.end_offset + skip * width,
            count: line.count - skip,
          });
      }
    return {
      ...batch,
      lines,
      dropped_lines: obsolete
        ? 0
        : Math.max(0, batch.dropped_lines - (same && batch.snapshot ? this.dropped : 0)),
      skipped_long_lines: obsolete
        ? 0
        : Math.max(0, batch.skipped_long_lines - (same && batch.snapshot ? this.skipped : 0)),
      retry_gap: batch.retry_gap
        ? {
            ...batch.retry_gap,
            dropped_notifications: obsolete
              ? 0
              : Math.max(0, batch.retry_gap.dropped_notifications - (same ? this.gapNotices : 0)),
            known_missing_occurrences: obsolete
              ? 0
              : Math.max(
                  0,
                  batch.retry_gap.known_missing_occurrences - (same ? this.gapOccurrences : 0),
                ),
          }
        : undefined,
    };
  }

  admit(batch: BashWatchLineBatch): void {
    const pendingID = batch.pending_id ?? "legacy";
    if (this.pendingID !== pendingID) {
      this.pendingID = pendingID;
      this.version = undefined;
      this.ends = { stdout: 0, stderr: 0, pty: 0, output: 0 };
      this.dropped = 0;
      this.skipped = 0;
      this.gapNotices = 0;
      this.gapOccurrences = 0;
    }
    if (
      !batch.snapshot &&
      batch.version !== undefined &&
      this.version !== undefined &&
      batch.version <= this.version
    )
      return;
    for (const line of batch.lines) {
      if (knownStream(line.stream)) {
        const end = line.offset + line.count * Math.max(1, line.end_offset - line.offset);
        this.ends[line.stream] = Math.max(this.ends[line.stream], end);
      }
    }
    this.dropped = batch.snapshot
      ? Math.max(this.dropped, batch.dropped_lines)
      : this.dropped + batch.dropped_lines;
    this.skipped = batch.snapshot
      ? Math.max(this.skipped, batch.skipped_long_lines)
      : this.skipped + batch.skipped_long_lines;
    this.gapNotices = Math.max(this.gapNotices, batch.retry_gap?.dropped_notifications ?? 0);
    this.gapOccurrences = Math.max(
      this.gapOccurrences,
      batch.retry_gap?.known_missing_occurrences ?? 0,
    );
    if (batch.version !== undefined)
      this.version = Math.max(this.version ?? batch.version, batch.version);
  }
}

export function watchBatchHasContent(batch: BashWatchLineBatch): boolean {
  return (
    batch.lines.length > 0 ||
    batch.dropped_lines > 0 ||
    batch.skipped_long_lines > 0 ||
    (batch.retry_gap?.dropped_notifications ?? 0) > 0
  );
}

type BatchMatch = {
  task_id: string;
  watch_id: string;
  match_offset: number;
  batch?: BashWatchLineBatch;
};

/** Watermarks survive failed acknowledgements, but clear on a confirmed ack or absence from the pending list. */
export class WatchBatchAdmissions {
  private readonly watches = new Map<string, WatchBatchProgress>();

  private key(match: Pick<BatchMatch, "task_id" | "watch_id">): string {
    return JSON.stringify([match.task_id, match.watch_id]);
  }

  unseen<T extends BatchMatch>(matches: readonly T[]): T[] {
    const visible: T[] = [];
    for (const match of matches) {
      if (!match.batch) {
        visible.push(match);
        continue;
      }
      const progress = this.watches.get(this.key(match)) ?? new WatchBatchProgress();
      const batch = progress.unseen({ ...match.batch, version: match.match_offset });
      if (watchBatchHasContent(batch)) visible.push({ ...match, batch });
    }
    return visible;
  }

  admit(matches: readonly BatchMatch[]): void {
    for (const match of matches) {
      if (!match.batch) continue;
      const key = this.key(match);
      const progress = this.watches.get(key) ?? new WatchBatchProgress();
      progress.admit({ ...match.batch, version: match.match_offset });
      this.watches.set(key, progress);
    }
  }

  confirmed(targets: readonly BashAckTarget[]): void {
    for (const target of targets) {
      if ("watch_id" in target) {
        const key = this.key(target);
        if (this.watches.get(key)?.admittedVersion === target.match_offset)
          this.watches.delete(key);
      } else {
        for (const key of this.watches.keys()) {
          if ((JSON.parse(key) as [string, string])[0] === target.task_id) this.watches.delete(key);
        }
      }
    }
  }

  reconcile(pending: readonly BatchMatch[]): void {
    const present = new Set(pending.map((match) => this.key(match)));
    for (const key of this.watches.keys()) if (!present.has(key)) this.watches.delete(key);
  }

  get size(): number {
    return this.watches.size;
  }
}
