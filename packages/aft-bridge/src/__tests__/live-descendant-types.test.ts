import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { LIVE_DESCENDANTS_UNAVAILABLE_REASONS } from "../index.js";

/** The variants of a Rust enum, in declaration order, as serde's snake_case. */
function rustEnumVariants(source: string, name: string): string[] {
  const start = source.indexOf(`pub enum ${name} {`);
  expect(start).toBeGreaterThanOrEqual(0);
  const body = source.slice(start, source.indexOf("\n}", start));
  return [...body.matchAll(/^ {4}([A-Z][A-Za-z0-9]*),$/gm)].map((match) =>
    (match[1] as string).replace(/(?<!^)([A-Z])/g, "_$1").toLowerCase(),
  );
}

describe("live descendant types", () => {
  test("the unavailable reasons match the reasons AFT sends", () => {
    const source = readFileSync(
      new URL("../../../../crates/aft/src/bash_background/process.rs", import.meta.url),
      "utf8",
    );
    const variants = rustEnumVariants(source, "LiveDescendantsUnavailable");
    expect(variants).toContain("listing_incomplete");
    expect([...LIVE_DESCENDANTS_UNAVAILABLE_REASONS]).toEqual(variants);
  });
});
