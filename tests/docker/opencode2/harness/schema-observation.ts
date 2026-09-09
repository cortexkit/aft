import { mkdir, writeFile } from "node:fs/promises";
import { dirname } from "node:path";

import type { RecordedMockExchange, ScenarioDefinition } from "./types.js";

export const HOST_SCHEMA_REJECTION_PROBE: ScenarioDefinition = {
  schema_version: 1,
  id: "read/T2/host_schema_rejection_probe",
  tool: "read",
  trajectory: "T2",
  execution: "standalone",
  prompt: "Call read with the supplied arguments, then report the result.",
  auto: true,
  error_origin: "host",
  subcase: "invalid_arguments",
  turns: [
    {
      label: "schema-invalid-call",
      response: {
        kind: "tool_calls",
        calls: [
          {
            id: "host-schema-rejection",
            name: "read",
            arguments: {},
            non_mutating_evidence: { reason: "host rejects missing required arguments before AFT" },
          },
        ],
      },
    },
    { label: "schema-rejection-observed", response: { kind: "text", content: "Probe complete." } },
  ],
  expected_turns: ["schema-invalid-call", "schema-rejection-observed"],
};

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

function toolResultFromExchange(
  exchange: RecordedMockExchange,
): { text: string; event: Record<string, unknown> } | undefined {
  const messages = asRecord(exchange.request)?.messages;
  if (!Array.isArray(messages)) return undefined;
  for (const message of messages.toReversed()) {
    const record = asRecord(message);
    if (record?.role !== "tool" || typeof record.content !== "string") continue;
    return { text: record.content, event: record };
  }
  return undefined;
}

export async function writeHostSchemaRejectionObservation(options: {
  outputPath: string;
  hostVersion: string;
  runId: string;
  scenarioId: string;
  hostStream: string;
  exchanges: readonly RecordedMockExchange[];
}): Promise<void> {
  const observed = options.exchanges.map(toolResultFromExchange).find(Boolean);
  if (!observed)
    throw new Error("host schema-rejection probe produced no agent-visible tool result");
  await mkdir(dirname(options.outputPath), { recursive: true });
  await writeFile(
    options.outputPath,
    `${JSON.stringify(
      {
        schema_version: 1,
        host_version: options.hostVersion,
        observed_run_id: options.runId,
        scenario_id: options.scenarioId,
        observation: {
          agent_visible_text: observed.text,
          json_event: observed.event,
          host_json_stream: options.hostStream,
        },
      },
      null,
      2,
    )}\n`,
  );
}
