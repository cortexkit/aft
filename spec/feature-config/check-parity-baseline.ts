import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { join, resolve } from "node:path";

// Pin both the IDs and their count: replacing a lost old case with a new one
// must not turn a parity regression into a passing count check.
const baseline = JSON.parse(
  readFileSync(new URL("./parity-baseline.json", import.meta.url), "utf8"),
) as { count: number; fixture_ids: string[] };
const fixturesRoot = resolve(
  process.env.AFT_PARITY_FIXTURES_ROOT ?? "crates/aft/tests/fixtures/config_parity",
);

if (
  !Number.isSafeInteger(baseline.count) ||
  baseline.count < 1 ||
  baseline.fixture_ids.length !== baseline.count ||
  new Set(baseline.fixture_ids).size !== baseline.count ||
  baseline.fixture_ids.some((id) => !/^[a-z0-9_]+$/.test(id))
) {
  throw new Error("invalid pinned parity baseline");
}
const missing = baseline.fixture_ids.filter(
  (id) => !existsSync(join(fixturesRoot, id, "expected.json")),
);
const actual = readdirSync(fixturesRoot).filter((id) =>
  statSync(join(fixturesRoot, id)).isDirectory(),
).length;
if (missing.length > 0) {
  throw new Error(`missing baseline cases: ${missing.join(", ")}`);
}
if (actual < baseline.count) {
  throw new Error(`parity coverage regressed: ${actual} < ${baseline.count}`);
}
console.log(`parity baseline: ${baseline.count} pinned IDs present, ${actual} cases total`);
