import { fail } from "./errors.js";
import type { ScenarioDefinition } from "./types.js";

export interface ResolvedTransportDeadWindow {
  from_turn: string;
  to_turn: string;
  from_index: number;
  to_index: number;
}

function defaultFallbackTurn(scenario: ScenarioDefinition): string | undefined {
  return scenario.turns.find(
    (turn) =>
      turn.response.kind === "tool_calls" &&
      turn.response.calls.some((call) => call.name === "bash"),
  )?.label;
}

export function resolveTransportDeadWindow(
  scenario: ScenarioDefinition,
): ResolvedTransportDeadWindow | undefined {
  const fallbackTurn = defaultFallbackTurn(scenario);
  const declaration = scenario.transport_dead ??
    (scenario.id.startsWith("bash/T3/fallback_") && fallbackTurn
      ? { from_turn: fallbackTurn, to_turn: fallbackTurn }
      : undefined);
  if (!declaration) return undefined;

  const fromIndex = scenario.turns.findIndex((turn) => turn.label === declaration.from_turn);
  const toIndex = scenario.turns.findIndex((turn) => turn.label === declaration.to_turn);
  if (fromIndex === -1 || toIndex === -1 || toIndex < fromIndex) {
    fail("scenario_invalid", `${scenario.id}: invalid transport-dead turn window`, {
      transport_dead: declaration,
    });
  }
  return {
    ...declaration,
    from_index: fromIndex,
    to_index: toIndex,
  };
}

export function transportDeadAtTurn(
  scenario: ScenarioDefinition,
  window: ResolvedTransportDeadWindow,
  turnLabel: string,
): boolean {
  const index = scenario.turns.findIndex((turn) => turn.label === turnLabel);
  if (index === -1) fail("scenario_invalid", `${scenario.id}: unknown turn ${turnLabel}`);
  return index >= window.from_index && index <= window.to_index;
}
