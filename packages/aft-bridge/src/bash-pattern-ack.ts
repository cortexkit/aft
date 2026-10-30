import type { BashPatternMatchAck } from "./bridge.js";

export type BashAckTarget = { task_id: string } | BashPatternMatchAck;

type PatternIdentity = BashPatternMatchAck & { reason?: string; ackCompletionOnDelivery?: boolean };

export function bashPatternAckTarget(match: PatternIdentity): BashAckTarget {
  return match.reason === "task_exit"
    ? { task_id: match.task_id }
    : { task_id: match.task_id, watch_id: match.watch_id, match_offset: match.match_offset };
}

export function bashAckKey(target: BashAckTarget): string {
  return "watch_id" in target
    ? `watch:${JSON.stringify([target.task_id, target.watch_id, target.match_offset])}`
    : target.task_id;
}

export function bashAckTargetFromKey(key: string): BashAckTarget {
  if (!key.startsWith("watch:")) return { task_id: key };
  const [task_id, watch_id, match_offset] = JSON.parse(key.slice("watch:".length)) as [
    string,
    string,
    number,
  ];
  return { task_id, watch_id, match_offset };
}

export function bashAckTargets(
  completions: readonly { task_id: string }[],
  matches: readonly PatternIdentity[],
): BashAckTarget[] {
  const targets = new Map<string, BashAckTarget>();
  for (const completion of completions) {
    const target = { task_id: completion.task_id };
    targets.set(bashAckKey(target), target);
  }
  for (const match of matches) {
    if (!match.ackCompletionOnDelivery) continue;
    const target = bashPatternAckTarget(match);
    targets.set(bashAckKey(target), target);
  }
  return [...targets.values()];
}

export function bashAckRequest(targets: readonly BashAckTarget[]): {
  task_ids?: string[];
  matches?: BashPatternMatchAck[];
} {
  const unique = new Map(targets.map((target) => [bashAckKey(target), target]));
  const task_ids: string[] = [];
  const matches: BashPatternMatchAck[] = [];
  for (const target of unique.values()) {
    if ("watch_id" in target) matches.push(target);
    else task_ids.push(target.task_id);
  }
  return {
    ...(task_ids.length > 0 ? { task_ids } : {}),
    ...(matches.length > 0 ? { matches } : {}),
  };
}

export function bashAcksConfirmed(
  response: Record<string, unknown>,
  targets: readonly BashAckTarget[],
): boolean {
  if (response.success === false) return false;
  const matches = Array.isArray(response.acked_matches) ? response.acked_matches : [];
  return targets.every(
    (target) =>
      !("watch_id" in target) ||
      matches.some((entry) => {
        if (entry === null || typeof entry !== "object") return false;
        const ack = entry as Partial<BashPatternMatchAck>;
        return (
          ack.task_id === target.task_id &&
          ack.watch_id === target.watch_id &&
          ack.match_offset === target.match_offset
        );
      }),
  );
}
