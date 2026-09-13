/**
 * Offline tests for the content-keyed legacy-vocabulary allowlist.
 *
 * No git, no filesystem fixtures: every case scans in-memory lines with
 * scanLinesForOccurrences and classifies them against a synthetic allowlist
 * with classifyOccurrenceDrift, so the suite runs anywhere bun runs.
 */
import { describe, expect, test } from "bun:test";
import {
  classifyOccurrenceDrift,
  legacyOccurrenceKey,
  scanLinesForOccurrences,
} from "./audit-v049-agent-surface.ts";

const PATH = "packages/example/sessions.ts";

interface TestEntry {
  path: string;
  location: string;
  line?: number;
  column?: number;
  text?: string;
  ordinal?: number;
  token: string;
  class: string;
  reason: string;
}

function entriesForLines(path: string, lines: string[]): TestEntry[] {
  return scanLinesForOccurrences(path, lines).map((occurrence) => ({
    path: occurrence.path,
    location: occurrence.location,
    line: occurrence.line,
    column: occurrence.column,
    text: occurrence.text,
    ordinal: occurrence.ordinal,
    token: occurrence.token,
    class: "internal-compatibility",
    reason: "test allowlist entry",
  }));
}

const BASE_LINES = [
  "import { load } from './store';",
  "const session = openSession(filePath);",
  "await persist(session, toFile);",
];

describe("content-keyed legacy-vocabulary allowlist", () => {
  test("inserting a line above both occurrences keeps the audit green", () => {
    const allowlisted = entriesForLines(PATH, BASE_LINES);
    expect(allowlisted).toHaveLength(2);
    const shifted = entriesForLines(PATH, ["// added comment", ...BASE_LINES]);
    expect(classifyOccurrenceDrift(shifted, allowlisted)).toEqual([]);
  });

  test("the same shift goes red on the old line:col scheme", () => {
    const allowlisted = entriesForLines(PATH, BASE_LINES);
    const shifted = entriesForLines(PATH, ["// added comment", ...BASE_LINES]);
    const before = new Set(allowlisted.map(legacyOccurrenceKey));
    const after = new Set(shifted.map(legacyOccurrenceKey));
    expect(after).not.toEqual(before);
  });

  test("changing the text of one occurrence is not allowlisted and stales the old entry", () => {
    const allowlisted = entriesForLines(PATH, BASE_LINES);
    const changedLines = [...BASE_LINES];
    changedLines[1] = "const session = openSession(filePath, { strict: true });";
    const failures = classifyOccurrenceDrift(entriesForLines(PATH, changedLines), allowlisted);
    expect(failures.some((failure) => failure.includes("is not allowlisted"))).toBe(true);
    expect(failures.some((failure) => failure.includes("is stale in the allowlist"))).toBe(true);
  });

  test("a duplicated identical line yields a second ordinal that is not allowlisted", () => {
    const allowlisted = entriesForLines(PATH, BASE_LINES);
    const duplicated = entriesForLines(PATH, [...BASE_LINES, BASE_LINES[1]]);
    const failures = classifyOccurrenceDrift(duplicated, allowlisted);
    expect(
      failures.some((failure) => failure.includes("#2") && failure.includes("is not allowlisted")),
    ).toBe(true);
    expect(failures.some((failure) => failure.includes("stale"))).toBe(false);
  });

  test("re-indenting a line keeps its pin", () => {
    const allowlisted = entriesForLines(PATH, BASE_LINES);
    const reindented = [...BASE_LINES];
    reindented[1] = `    ${BASE_LINES[1].trim()}   `;
    expect(classifyOccurrenceDrift(entriesForLines(PATH, reindented), allowlisted)).toEqual([]);
  });
});
