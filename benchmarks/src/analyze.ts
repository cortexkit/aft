#!/usr/bin/env bun
/**
 * Analyze and compare benchmark results from saved JSON files.
 *
 * Usage:
 *   bun run analyze results/benchmark-2024-01-01.json
 *   bun run analyze results/run1.json results/run2.json  # Compare two runs
 */
import { readFileSync } from "fs";
import type { BenchmarkReport } from "./config";
import { printReport } from "./reporter";

const files = process.argv.slice(2);

if (files.length === 0) {
  console.log("Usage: bun run analyze <results.json> [results2.json ...]");
  process.exit(1);
}

for (const file of files) {
  try {
    const data = readFileSync(file, "utf-8");
    const report: BenchmarkReport = JSON.parse(data);
    console.log(`\nFile: ${file}`);
    printReport(report);
  } catch (e) {
    console.error(`Failed to read ${file}:`, e);
  }
}
