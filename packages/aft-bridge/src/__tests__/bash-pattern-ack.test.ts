import { expect, test } from "bun:test";
import {
  bashAckKey,
  bashAckRequest,
  bashAcksConfirmed,
  bashAckTargetFromKey,
  bashAckTargets,
} from "../bash-pattern-ack.js";

test("pattern acknowledgements keep sibling watches and newer offsets distinct", () => {
  const targets = bashAckTargets(
    [{ task_id: "task" }],
    [
      { task_id: "task", watch_id: "one", match_offset: 7, ackCompletionOnDelivery: true },
      { task_id: "task", watch_id: "two", match_offset: 7, ackCompletionOnDelivery: true },
      { task_id: "task", watch_id: "one", match_offset: 8, ackCompletionOnDelivery: true },
      { task_id: "task", watch_id: "one", match_offset: 7, ackCompletionOnDelivery: true },
    ],
  );
  expect(bashAckRequest(targets)).toEqual({
    task_ids: ["task"],
    matches: [
      { task_id: "task", watch_id: "one", match_offset: 7 },
      { task_id: "task", watch_id: "two", match_offset: 7 },
      { task_id: "task", watch_id: "one", match_offset: 8 },
    ],
  });
  for (const target of targets) expect(bashAckTargetFromKey(bashAckKey(target))).toEqual(target);
});

test("pattern acknowledgements require the exact returned version", () => {
  const targets = [{ task_id: "task", watch_id: "one", match_offset: 7 }];
  expect(bashAcksConfirmed({ success: true, acked_task_ids: ["task"] }, targets)).toBe(false);
  expect(
    bashAcksConfirmed(
      { success: true, acked_matches: [{ task_id: "task", watch_id: "one", match_offset: 8 }] },
      targets,
    ),
  ).toBe(false);
  expect(
    bashAcksConfirmed(
      { success: true, acked_matches: [{ task_id: "task", watch_id: "two", match_offset: 7 }] },
      targets,
    ),
  ).toBe(false);
  expect(bashAcksConfirmed({ success: true, acked_matches: targets }, targets)).toBe(true);
});

test("task exits keep task-level acknowledgement and ordinary matches do not", () => {
  expect(
    bashAckRequest(
      bashAckTargets(
        [],
        [
          {
            task_id: "task-exit",
            watch_id: "exit",
            match_offset: 0,
            reason: "task_exit",
            ackCompletionOnDelivery: true,
          },
          {
            task_id: "task-pattern",
            watch_id: "watch",
            match_offset: 3,
            reason: "pattern_match",
            ackCompletionOnDelivery: true,
          },
        ],
      ),
    ),
  ).toEqual({
    task_ids: ["task-exit"],
    matches: [{ task_id: "task-pattern", watch_id: "watch", match_offset: 3 }],
  });
});
