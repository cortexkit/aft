import { describe, expect, test } from "bun:test";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

// The CLI ships as one bundle, so a static `import ... from "node:sqlite"`
// anywhere in src/ becomes a top-level link of the whole entry point: on
// Node < 22.5 every command, including `setup`, dies with
// ERR_UNKNOWN_BUILTIN_MODULE before it can name the requirement (#306).
// `node:sqlite` is loaded on use only; type-only imports are erased.
function sourceFiles(dir: string, out: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) {
      if (entry !== "__tests__") sourceFiles(path, out);
    } else if (path.endsWith(".ts") && !path.endsWith(".test.ts")) {
      out.push(path);
    }
  }
  return out;
}

describe("node:sqlite is never linked at CLI load", () => {
  test("no runtime import of node:sqlite in src/", () => {
    const root = join(import.meta.dir, "..");
    const offenders: string[] = [];
    for (const file of sourceFiles(root)) {
      const text = readFileSync(file, "utf8");
      for (const line of text.split("\n")) {
        const trimmed = line.trim();
        if (
          /^import\s+(?!type\b)[^;]*from\s+["']node:sqlite["']/.test(trimmed) ||
          /^import\s+["']node:sqlite["']/.test(trimmed)
        ) {
          offenders.push(`${file}: ${trimmed}`);
        }
      }
    }
    expect(offenders).toEqual([]);
  });
});
