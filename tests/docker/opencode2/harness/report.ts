import type { ScenarioResult } from "./types.js";
import { TRAJECTORIES } from "./types.js";
import type { ApplicabilityMatrix, MatrixClassification } from "./validation.js";

export type RowDisposition = "expected_fail" | "fail" | "n/a" | "pass";

/**
 * Failures that say the run never established the conditions an
 * expected-failure label is about.
 *
 * These are all about the harness itself: which binary ran, which plugin it
 * loaded, whether the scenario stayed inside its own roots, and whether the
 * pinned-host contracts and the matrix were readable. A product outcome
 * belongs to the label; a run that could not set up the product does not.
 */
const HARNESS_INTEGRITY_CODES = new Set([
  "contract_uncaptured",
  "executable_provenance",
  "matrix_invalid",
  "plugin_source_invalid",
  "scenario_invalid",
]);

function breaksHarnessIntegrity(result: ScenarioResult): boolean {
  const failure = result.failure;
  return (
    failure?.unsuppressible === true &&
    HARNESS_INTEGRITY_CODES.has(failure.code.replace(/:.*$/, ""))
  );
}

/**
 * How a matrix cell reads once its scenarios have run.
 *
 * An expected-failure label excuses a product failure, not a broken harness,
 * so a harness-integrity failure counts as a plain failure and the label
 * cannot hide it.
 */
export function parentDisposition(
  classification: MatrixClassification,
  results: readonly ScenarioResult[],
): RowDisposition {
  if (classification.startsWith("n/a:")) return "n/a";
  if (classification.startsWith("expected_fail:")) {
    const failed = results.filter((result) => result.status === "failed");
    if (failed.some(breaksHarnessIntegrity)) return "fail";
    return failed.length > 0 ? "expected_fail" : "fail";
  }
  return results.length > 0 && results.every((result) => result.status === "passed")
    ? "pass"
    : "fail";
}

/**
 * Render every matrix cell, its scenarios, and the run's four counts.
 *
 * The counts are the verdict the table exists to produce: without them a
 * reader has to tally a hundred-odd lines by hand to learn whether a run
 * moved. Every cell in scope is rendered whatever happened to the ones before
 * it, because a report that stops at the first problem cannot report a
 * verdict.
 */
export function reportTable(
  matrix: ApplicabilityMatrix,
  results: readonly ScenarioResult[],
  selector?: string,
): { text: string; failed: boolean } {
  const lines = ["tool | trajectory | applicable/n-a(reason) | pass/fail/expected_fail(issue)"];
  let failed = false;
  const rowCounts: Record<RowDisposition, number> = {
    pass: 0,
    fail: 0,
    expected_fail: 0,
    "n/a": 0,
  };
  for (const row of matrix.rows.toSorted((left, right) => left.tool.localeCompare(right.tool))) {
    for (const trajectory of TRAJECTORIES) {
      const parent = `${row.tool}/${trajectory}`;
      if (selector && parent !== selector.replace(/\/$/, "") && !selector.startsWith(`${parent}/`)) {
        continue;
      }
      const classification = row.trajectories[trajectory];
      const children = results.filter(
        (result) => result.id === parent || result.id.startsWith(`${parent}/`),
      );
      const disposition = parentDisposition(classification, children);
      if (disposition === "fail") failed = true;
      rowCounts[disposition] += 1;
      const applicability = classification.startsWith("n/a:") ? classification : "applicable";
      const renderedDisposition =
        disposition === "expected_fail"
          ? classification
          : disposition === "n/a"
            ? "n/a"
            : disposition;
      lines.push(`${row.tool} | ${trajectory} | ${applicability} | ${renderedDisposition}`);
      for (const child of children) {
        lines.push(
          `  ${child.id} | ${child.status}${child.failure ? ` | ${child.failure.code}` : ""} | ${child.elapsed_ms ?? 0}ms`,
        );
      }
    }
  }
  const rowTotal = rowCounts.pass + rowCounts.fail + rowCounts.expected_fail + rowCounts["n/a"];
  lines.push(
    `rows: ${rowCounts.pass} passed, ${rowCounts.fail} failed, ` +
      `${rowCounts.expected_fail} expected-failed, ${rowCounts["n/a"]} not-applicable (of ${rowTotal})`,
  );
  const scenariosPassed = results.filter((result) => result.status === "passed").length;
  lines.push(
    `scenarios: ${scenariosPassed} passed, ${results.length - scenariosPassed} failed ` +
      `(of ${results.length})`,
  );
  return { text: lines.join("\n"), failed };
}
