import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

import type { RecordedMockExchange, ScenarioDefinition } from "./types.js";

export class ScenarioForensics {
  readonly directory: string;

  constructor(runRoot: string, scenarioId: string) {
    this.directory = join(runRoot, "forensics", ...scenarioId.split("/"));
  }

  async initialize(scenario: ScenarioDefinition): Promise<void> {
    await mkdir(this.directory, { recursive: true });
    await this.writeJson("scenario.json", scenario);
  }

  async writeJson(name: string, value: unknown): Promise<void> {
    await writeFile(join(this.directory, name), `${JSON.stringify(value, null, 2)}\n`);
  }

  async writeText(name: string, value: string): Promise<void> {
    await writeFile(join(this.directory, name), value);
  }

  async writeExchanges(exchanges: readonly RecordedMockExchange[]): Promise<void> {
    await writeFile(
      join(this.directory, "mock-exchanges.ndjson"),
      `${exchanges.map((exchange) => JSON.stringify(exchange)).join("\n")}\n`,
    );
  }

  async recordFailure(error: unknown): Promise<void> {
    const record =
      error instanceof Error
        ? { name: error.name, message: error.message, stack: error.stack }
        : { name: "NonError", message: String(error) };
    await this.writeJson("failure.json", record);
  }
}
