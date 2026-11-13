// Small wall/awake sampling differences are ordinary clock noise. Accumulate
// them, but only treat a full second of forward skew as suspend (or a wall
// correction). JavaScript cannot distinguish those two causes.
export const SUSPEND_SKEW_MS = 1_000;
export const RESUME_ALLOWANCE_MS = 10_000;
export const DEADLINE_POLL_MS = 250;

export class WaitClock {
  private wall: number;
  private awake: number;
  private skew = 0;
  private elapsed: number;
  suspendCount = 0;
  lastSuspendEnd = Number.NEGATIVE_INFINITY;

  constructor(
    private readonly wallNow: () => number = Date.now,
    private readonly awakeNow: () => number = () => performance.now(),
  ) {
    this.wall = wallNow();
    this.awake = awakeNow();
    this.elapsed = this.awake;
  }

  now(): number {
    const wall = this.wallNow();
    const awake = this.awakeNow();
    const active = Math.max(0, awake - this.awake);
    // Reset negative skew rather than letting a backward wall adjustment buy
    // more wait time, or charging its later recovery as another suspend.
    this.skew = Math.max(0, this.skew + wall - this.wall - active);
    this.elapsed += active;
    if (this.skew >= SUSPEND_SKEW_MS) {
      this.elapsed += this.skew;
      this.lastSuspendEnd = this.elapsed;
      this.skew = 0;
      this.suspendCount++;
    }
    this.wall = wall;
    this.awake = awake;
    return this.elapsed;
  }
}

/** One caller budget, shared by connection, route admission and reply delivery. */
export class RequestDeadline {
  private deadline: number;
  private readonly suspendCount: number;
  resumed = false;

  constructor(
    private readonly clock: WaitClock,
    timeoutMs: number,
    private readonly allowResume = true,
  ) {
    this.deadline = clock.now() + timeoutMs;
    this.suspendCount = clock.suspendCount;
  }

  remaining(): number {
    const now = this.clock.now();
    if (
      now >= this.deadline &&
      this.allowResume &&
      !this.resumed &&
      this.clock.suspendCount > this.suspendCount &&
      this.clock.lastSuspendEnd >= this.deadline
    ) {
      this.resumed = true;
      this.deadline = now + RESUME_ALLOWANCE_MS;
    }
    return Math.max(0, this.deadline - now);
  }
}

export interface WaitScheduler {
  setTimeout(callback: () => void, ms: number): unknown;
  clearTimeout(timer: unknown): void;
}

export function deadlineTimer(
  scheduler: WaitScheduler,
  remaining: () => number,
  expired: () => void,
): () => void {
  let timer: unknown;
  const check = () => {
    const left = remaining();
    if (left <= 0) expired();
    else timer = scheduler.setTimeout(check, Math.min(left, DEADLINE_POLL_MS));
  };
  check();
  return () => scheduler.clearTimeout(timer);
}
