import type { WaitScheduler } from "./wait-clock.js";

const BATCH_SIZE = 4;
const BATCH_GAP_MS = 20;
const BATCH_JITTER_MS = 20;

type Waiter<Client> = {
  dueAt: number;
  resolve: (client: Client | null) => void;
  reject: (error: unknown) => void;
};

/** One retry timer and one acquisition at a time for a pool's shared connection. */
export class BgReattachSchedule<Client> {
  private readonly waiters = new Set<Waiter<Client>>();
  private alarm?: { dueAt: number; cancel: () => void };
  private acquiring = false;
  private nextBatchAt = Number.NEGATIVE_INFINITY;
  private injectedTime = Number.NEGATIVE_INFINITY;

  constructor(
    private readonly clock: WaitScheduler,
    private readonly now: () => number,
    private readonly acquire: () => Promise<Client>,
    private readonly sleep?: (ms: number) => Promise<void>,
  ) {}

  wait(delay: number): { ready: Promise<Client | null>; cancel: () => void } {
    let waiter!: Waiter<Client>;
    const ready = new Promise<Client | null>((resolve, reject) => {
      waiter = { dueAt: this.time() + delay, resolve, reject };
      this.waiters.add(waiter);
      if (!this.alarm || this.alarm.dueAt > Math.max(waiter.dueAt, this.nextBatchAt)) {
        this.arm();
      }
    });
    return {
      ready,
      cancel: () => {
        if (!this.waiters.delete(waiter)) return;
        waiter.resolve(null);
        if (this.waiters.size === 0) this.cancelAlarm();
      },
    };
  }

  private time(): number {
    return Math.max(this.now(), this.injectedTime);
  }

  private cancelAlarm(): void {
    this.alarm?.cancel();
    this.alarm = undefined;
  }

  private arm(): void {
    this.cancelAlarm();
    if (this.acquiring || this.waiters.size === 0) return;
    let firstDueAt = Number.POSITIVE_INFINITY;
    for (const waiter of this.waiters) firstDueAt = Math.min(firstDueAt, waiter.dueAt);
    const dueAt = Math.max(firstDueAt, this.nextBatchAt);
    const delay = Math.max(0, Math.ceil(dueAt - this.time()));
    const alarm = { dueAt, cancel: () => {} };
    this.alarm = alarm;
    const wake = (): void => {
      if (this.alarm !== alarm) return;
      this.alarm = undefined;
      // Tests can inject a sleeper that settles without advancing the clock.
      if (this.sleep) this.injectedTime = Math.max(this.injectedTime, dueAt);
      void this.releaseBatch();
    };
    if (this.sleep) {
      // The sleeper itself may be uncancellable. Ignore its result once disarmed.
      const sleep = this.sleep;
      void Promise.resolve()
        .then(() => {
          if (this.alarm === alarm) return sleep(delay);
        })
        .then(wake, (error) => {
          if (this.alarm !== alarm) return;
          this.alarm = undefined;
          for (const waiter of this.waiters) waiter.reject(error);
          this.waiters.clear();
        });
    } else {
      const timer = this.clock.setTimeout(wake, delay);
      alarm.cancel = () => this.clock.clearTimeout(timer);
    }
  }

  private async releaseBatch(): Promise<void> {
    if (this.waiters.size === 0) return;
    this.acquiring = true;
    try {
      // Start spacing route opens only after the client is acquired. Otherwise
      // all groups waiting on a slow, single-flight connect could open together.
      const client = await this.acquire();
      const now = this.time();
      this.nextBatchAt = now + BATCH_GAP_MS + Math.floor(Math.random() * BATCH_JITTER_MS);
      let released = 0;
      for (const waiter of this.waiters) {
        if (waiter.dueAt > now) continue;
        this.waiters.delete(waiter);
        waiter.resolve(client);
        if (++released === BATCH_SIZE) break;
      }
    } catch (error) {
      // No routes opened. Every due waiter can back off from this same failure.
      const now = this.time();
      for (const waiter of this.waiters) {
        if (waiter.dueAt > now) continue;
        this.waiters.delete(waiter);
        waiter.reject(error);
      }
    } finally {
      this.acquiring = false;
      this.arm();
    }
  }
}
