import { describe, expect, test } from "bun:test";
import { RequestDeadline, WaitClock } from "../wait-clock.js";
import { ManualClock } from "./test-utils/manual-clock.js";

function timeline() {
  const scheduler = new ManualClock();
  const clock = new WaitClock(
    () => scheduler.now(),
    () => scheduler.awakeNow(),
  );
  return { scheduler, clock };
}

describe("suspend-inclusive wait clock", () => {
  test("counts suspend and accumulates small forward skew", () => {
    const { scheduler, clock } = timeline();
    scheduler.advance(250_000);
    expect(clock.now()).toBe(250_000);
    scheduler.suspend(2_000_000);
    expect(clock.now()).toBe(2_250_000);
    scheduler.stepWall(600);
    expect(clock.now()).toBe(2_250_000);
    scheduler.stepWall(400);
    expect(clock.now()).toBe(2_251_000);
  });

  test("a backward wall step gives no extra wait time", () => {
    const { scheduler, clock } = timeline();
    const deadline = new RequestDeadline(clock, 1_000);
    scheduler.stepWall(-50_000);
    expect(deadline.remaining()).toBe(1_000);
    scheduler.advance(1_000);
    expect(deadline.remaining()).toBe(0);
    expect(deadline.resumed).toBe(false);
  });

  test("suspend that ended before the deadline does not buy a later allowance", () => {
    const { scheduler, clock } = timeline();
    const deadline = new RequestDeadline(clock, 270_000);
    scheduler.suspend(2_000);
    expect(deadline.remaining()).toBe(268_000);
    scheduler.advance(268_000);
    expect(deadline.remaining()).toBe(0);
    expect(deadline.resumed).toBe(false);
  });

  test("silence expires after exactly one resume allowance, even after another jump", () => {
    const { scheduler, clock } = timeline();
    const deadline = new RequestDeadline(clock, 270_000);
    scheduler.advance(250_000);
    scheduler.suspend(2_000_000);
    expect(deadline.remaining()).toBe(10_000);
    scheduler.advance(9_999);
    expect(deadline.remaining()).toBe(1);
    scheduler.suspend(2_000_000);
    expect(deadline.remaining()).toBe(0);
  });

  test("a forward wall correction grants at most the same one allowance", () => {
    const { scheduler, clock } = timeline();
    const deadline = new RequestDeadline(clock, 1_000);
    scheduler.stepWall(10_000);
    expect(deadline.remaining()).toBe(10_000);
    scheduler.advance(10_000);
    expect(deadline.remaining()).toBe(0);
  });
});
