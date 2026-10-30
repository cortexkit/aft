import { expect, test } from "bun:test";
import {
  WatchBatchAdmissions,
  WatchBatchProgress,
  watchBatchHasContent,
} from "../bash-watch-progress.js";
import type { BashWatchLineBatch } from "../bridge.js";

function snapshot(id = "a"): BashWatchLineBatch {
  return {
    pending_id: id,
    version: 1,
    snapshot: true,
    lines: [{ stream: "stdout", offset: 0, end_offset: 8, text: "READY a", count: 1 }],
    dropped_lines: 7,
    skipped_long_lines: 2,
    first_dropped_offset: 8,
    first_dropped_stream: "stdout",
  };
}

test("watch progress dedups folded lines and cumulative counters after admission", () => {
  const progress = new WatchBatchProgress();
  const first = snapshot();
  expect(watchBatchHasContent(progress.unseen(first))).toBe(true);
  progress.admit(first);
  expect(watchBatchHasContent(progress.unseen(first))).toBe(false);
  const next = snapshot();
  next.lines[0].count = 3;
  next.dropped_lines = 10;
  next.skipped_long_lines = 3;
  next.version = 2;
  const unseen = progress.unseen(next);
  expect(unseen.lines).toEqual([
    { stream: "stdout", offset: 8, end_offset: 16, text: "READY a", count: 2 },
  ]);
  expect(unseen.dropped_lines).toBe(3);
  expect(unseen.skipped_long_lines).toBe(1);
});

test("retry gap-only batches are admitted once while newer gap versions remain visible", () => {
  const progress = new WatchBatchProgress();
  const first: BashWatchLineBatch = {
    ...snapshot(),
    lines: [],
    dropped_lines: 0,
    skipped_long_lines: 0,
    retry_gap: { reason: "retry_overflow", dropped_notifications: 2, known_missing_occurrences: 1 },
  };
  expect(watchBatchHasContent(progress.unseen(first))).toBe(true);
  progress.admit(first);
  expect(watchBatchHasContent(progress.unseen(first))).toBe(false);
  const next = {
    ...first,
    version: 2,
    retry_gap: { ...first.retry_gap!, dropped_notifications: 3, known_missing_occurrences: 2 },
  };
  expect(progress.unseen(next).retry_gap?.dropped_notifications).toBe(1);
  expect(progress.unseen(next).retry_gap?.known_missing_occurrences).toBe(1);
  progress.admit(next);
  expect(watchBatchHasContent(progress.unseen(next))).toBe(false);
});

test("legacy scalar output retained beside a retry gap is not repeated", () => {
  const progress = new WatchBatchProgress();
  const batch = snapshot();
  batch.lines[0].stream = "output";
  batch.retry_gap = {
    reason: "retry_overflow",
    dropped_notifications: 1,
    known_missing_occurrences: 0,
  };
  progress.admit(batch);
  expect(watchBatchHasContent(progress.unseen(batch))).toBe(false);
});

test("watch progress does not reserve rows before admission or across pending generations", () => {
  const progress = new WatchBatchProgress();
  expect(progress.unseen(snapshot()).lines).toHaveLength(1);
  expect(progress.unseen(snapshot()).lines).toHaveLength(1);
  progress.admit(snapshot());
  const fresh = snapshot("b");
  fresh.dropped_lines = 10;
  expect(progress.unseen(fresh).dropped_lines).toBe(10);
  expect(progress.unseen(fresh).lines).toHaveLength(1);
});

test("session batch admissions retain content watermarks while exact ack versions change", () => {
  const admissions = new WatchBatchAdmissions();
  const a = { task_id: "task", watch_id: "watch", match_offset: 0, batch: snapshot() };
  admissions.admit([a]);
  const b = {
    ...a,
    match_offset: 1,
    batch: {
      ...snapshot(),
      version: 1,
      dropped_lines: 10,
      lines: [
        ...snapshot().lines,
        { stream: "stderr", offset: 0, end_offset: 8, text: "READY b", count: 1 },
      ],
    },
  };
  const shown = admissions.unseen([b]);
  expect(shown[0]?.match_offset).toBe(1);
  expect(shown[0]?.batch?.lines.map((line) => line.text)).toEqual(["READY b"]);
  expect(shown[0]?.batch?.dropped_lines).toBe(3);
  admissions.admit([b]);
  admissions.confirmed([{ task_id: "task", watch_id: "watch", match_offset: 0 }]);
  expect(admissions.size).toBe(1);
  expect(admissions.unseen([b])).toEqual([]);
  admissions.confirmed([{ task_id: "task", watch_id: "watch", match_offset: 1 }]);
  expect(admissions.size).toBe(0);
});

test("session batch admission preview is not a reservation and durable absence retires state", () => {
  const admissions = new WatchBatchAdmissions();
  const a = { task_id: "task", watch_id: "watch", match_offset: 0, batch: snapshot() };
  expect(admissions.unseen([a])).toHaveLength(1);
  expect(admissions.unseen([a])).toHaveLength(1);
  expect(admissions.size).toBe(0);
  admissions.admit([a]);
  admissions.reconcile([{ ...a, match_offset: 1 }]);
  expect(admissions.size).toBe(1);
  admissions.reconcile([]);
  expect(admissions.size).toBe(0);
});
