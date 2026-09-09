import { fail } from "./errors.js";

export interface WatchLivenessEvidence {
  accepted_at?: number;
  armed_at?: number;
  first_match_at?: number;
  first_delivery_at?: number;
  matching_stdout_records: number;
  deliveries: number;
}

export class WatchPatternLiveness {
  readonly scenario: string;
  readonly pattern: RegExp;
  readonly setupMs: number;
  readonly initialDeliveryMs: number;
  readonly duplicateWindowMs: number;
  readonly evidence: WatchLivenessEvidence = {
    matching_stdout_records: 0,
    deliveries: 0,
  };

  constructor(options: {
    scenario: string;
    pattern: string;
    setupMs?: number;
    initialDeliveryMs?: number;
    duplicateWindowMs: number;
  }) {
    this.scenario = options.scenario;
    this.pattern = new RegExp(options.pattern);
    this.setupMs = options.setupMs ?? 30_000;
    this.initialDeliveryMs = options.initialDeliveryMs ?? 30_000;
    this.duplicateWindowMs = options.duplicateWindowMs;
  }

  accepted(at = Date.now()): void {
    if (this.evidence.accepted_at !== undefined)
      throw new Error(`${this.scenario}: call accepted twice`);
    this.evidence.accepted_at = at;
  }

  watchArmed(at = Date.now()): void {
    if (this.evidence.accepted_at === undefined)
      throw new Error(`${this.scenario}: watch armed before call acceptance`);
    this.evidence.armed_at = at;
  }

  taskStdout(line: string, at = Date.now()): void {
    if (!this.pattern.test(line)) return;
    this.pattern.lastIndex = 0;
    this.evidence.matching_stdout_records += 1;
    if (this.evidence.armed_at === undefined) {
      fail(
        "fixture_invalid",
        `${this.scenario}: matching task stdout preceded watch arm`,
        {},
        true,
      );
    }
    this.evidence.first_match_at ??= at;
  }

  delivery(at = Date.now()): void {
    if (this.evidence.first_match_at === undefined) {
      fail(
        "fixture_invalid",
        `${this.scenario}: delivery lacked a task-stdout antecedent`,
        {},
        true,
      );
    }
    this.evidence.deliveries += 1;
    this.evidence.first_delivery_at ??= at;
    if (this.evidence.deliveries > 1) {
      fail("duplicate_delivery", this.scenario, { evidence: this.evidence }, true);
    }
  }

  assertAt(now = Date.now()): void {
    const accepted = this.evidence.accepted_at;
    if (accepted === undefined) throw new Error(`${this.scenario}: call was not accepted`);
    if (this.evidence.armed_at === undefined && now - accepted >= this.setupMs) {
      fail("setup_timeout", `watch_arm:${this.scenario}`, { phase: "watch_arm" }, true);
    }
    if (this.evidence.first_match_at === undefined && now - accepted >= this.setupMs) {
      fail("setup_timeout", `first_match:${this.scenario}`, { phase: "first_match" }, true);
    }
    const firstMatch = this.evidence.first_match_at;
    if (
      firstMatch !== undefined &&
      this.evidence.first_delivery_at === undefined &&
      now - firstMatch >= this.initialDeliveryMs
    ) {
      fail("delivery_timeout", this.scenario, { antecedent: firstMatch }, true);
    }
    const firstDelivery = this.evidence.first_delivery_at;
    if (
      firstDelivery !== undefined &&
      now - firstDelivery >= this.duplicateWindowMs &&
      this.evidence.deliveries !== 1
    ) {
      fail("duplicate_delivery", this.scenario, { evidence: this.evidence }, true);
    }
  }

  assertComplete(now = Date.now()): void {
    this.assertAt(now);
    if (this.evidence.matching_stdout_records < 2) {
      fail(
        "fixture_invalid",
        `${this.scenario}: fixture must emit the matching task stdout twice`,
        { matching_stdout_records: this.evidence.matching_stdout_records },
        true,
      );
    }
    if (this.evidence.deliveries !== 1) {
      fail("delivery_timeout", this.scenario, { evidence: this.evidence }, true);
    }
  }
}

export class CompletionWakeLiveness {
  readonly scenario: string;
  readonly deadlineMs: number;
  readonly rearmWindowMs: number;
  readonly started = new Map<string, number>();
  readonly deliveries: Array<{ task_id: string; prompt_id: string; at: number }> = [];

  constructor(options: { scenario: string; deadlineMs?: number; rearmWindowMs: number }) {
    this.scenario = options.scenario;
    this.deadlineMs = options.deadlineMs ?? 30_000;
    this.rearmWindowMs = options.rearmWindowMs;
  }

  taskStarted(taskId: string, at = Date.now()): void {
    if (this.started.has(taskId)) throw new Error(`${this.scenario}: duplicate task ${taskId}`);
    this.started.set(taskId, at);
  }

  promptInjected(taskId: string, promptId: string, at = Date.now()): void {
    if (!this.started.has(taskId)) {
      fail("fixture_invalid", `${this.scenario}: wake for unknown task ${taskId}`, {}, true);
    }
    if (this.deliveries.some((delivery) => delivery.task_id === taskId)) {
      fail("duplicate_delivery", `${this.scenario}:${taskId}`, { prompt_id: promptId }, true);
    }
    if (this.deliveries.some((delivery) => delivery.prompt_id === promptId)) {
      fail("duplicate_delivery", `${this.scenario}: prompt ${promptId} was re-injected`, {}, true);
    }
    this.deliveries.push({ task_id: taskId, prompt_id: promptId, at });
  }

  assertDelivered(taskId: string, now = Date.now()): void {
    const started = this.started.get(taskId);
    if (started === undefined) throw new Error(`${this.scenario}: unknown task ${taskId}`);
    const deliveries = this.deliveries.filter((delivery) => delivery.task_id === taskId);
    if (deliveries.length === 0 && now - started >= this.deadlineMs) {
      fail("delivery_timeout", `${this.scenario}:${taskId}`, { started_at: started }, true);
    }
    if (deliveries.length !== 1) {
      fail("delivery_timeout", `${this.scenario}:${taskId}`, { deliveries }, true);
    }
    if (now - deliveries[0].at < this.rearmWindowMs) {
      throw new Error(`${this.scenario}: duplicate-detection window has not elapsed`);
    }
  }
}
