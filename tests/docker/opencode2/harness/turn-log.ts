import { readFile } from "node:fs/promises";

import { fail } from "./errors.js";

export async function readTurnLog(path: string): Promise<string[]> {
  try {
    return (await readFile(path, "utf8"))
      .split(/\r?\n/)
      .map((line) => line.trim())
      .filter(Boolean);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return [];
    throw error;
  }
}

export function assertTurnLog(expected: readonly string[], observed: readonly string[]): void {
  const missingAt = expected.findIndex((label, index) => observed[index] !== label);
  if (missingAt !== -1 || observed.length !== expected.length) {
    fail(
      "turn_log_incomplete",
      `expected ${expected.join(",")} but observed ${observed.join(",") || "<empty>"}`,
      {
        expected,
        observed,
        first_mismatch: missingAt === -1 ? Math.min(expected.length, observed.length) : missingAt,
      },
      true,
    );
  }
}
