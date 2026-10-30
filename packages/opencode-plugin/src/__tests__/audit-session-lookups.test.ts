import { afterEach, expect, test } from "bun:test";
import { Effect } from "effect";
import {
  _resetSessionDirectoryCacheForTest,
  getSessionDirectory,
  getSessionDirectoryCached,
  verifySessionDirectory,
} from "../shared/session-directory.js";
import { _resetSubagentCacheForTest, resolveIsSubagent } from "../shared/subagent-detect.js";

afterEach(() => {
  _resetSessionDirectoryCacheForTest();
  _resetSubagentCacheForTest();
});

test("late V1 directory success fills the current provisional cache", async () => {
  let release!: (value: { directory: string }) => void;
  const client = {
    session: {
      get: () =>
        new Promise<{ directory: string }>((resolve) => {
          release = resolve;
        }),
    },
  };
  expect(await getSessionDirectory(client, "late-current", "/fallback")).toBeNull();
  release({ directory: "/stored" });
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
  expect(getSessionDirectoryCached("late-current")).toBe("/stored");
});

test("verification misses are deduplicated and bounded", async () => {
  let calls = 0;
  const client = {
    session: {
      get: () => {
        calls++;
        return new Promise(() => {});
      },
    },
  };
  const first = verifySessionDirectory(client, "verify-hang");
  const second = verifySessionDirectory(client, "verify-hang");
  expect(calls).toBe(1);
  expect(await Promise.all([first, second])).toEqual([null, null]);
});

test("V2 subagent lookup interrupts abandoned Effect work", async () => {
  let interrupted = false;
  const client = {
    location: { directory: "/host" },
    session: {
      get: () =>
        Effect.never.pipe(
          Effect.onInterrupt(() =>
            Effect.sync(() => {
              interrupted = true;
            }),
          ),
        ),
    },
  };
  expect(await resolveIsSubagent(client, "v2-interrupt")).toBe(false);
  // The outer promise may win the deadline race; the Effect finalizer still must run.
  await new Promise((resolve) => setTimeout(resolve, 20));
  expect(interrupted).toBe(true);
});
