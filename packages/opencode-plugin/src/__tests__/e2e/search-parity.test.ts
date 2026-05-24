/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { globSync, realpathSync } from "node:fs";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type PreparedBinary,
  prepareBinary,
} from "./helpers.js";

type ParsedGrepMatch = {
  file: string;
  line: number;
  text: string;
};

type GrepCase = {
  name: string;
  pattern: string;
  caseSensitive?: boolean;
  include?: string;
  path?: string;
  ripgrepArgs?: string[];
};

type GlobCase = {
  name: string;
  pattern: string;
  path?: string;
};

const initialBinary = await prepareBinary();
const ripgrepAvailable = hasCommand("rg", ["--version"]);

if (!ripgrepAvailable) {
  console.warn("Skipping e2e search parity tests: ripgrep (rg) is not installed.");
}

const maybeDescribe = describe.skipIf(!initialBinary.binaryPath || !ripgrepAvailable);

const grepCases: GrepCase[] = [
  {
    name: "matches a simple literal",
    pattern: "SearchIndex",
  },
  {
    name: "matches regex alternation",
    pattern: "handle_grep|handle_glob",
  },
  {
    name: "matches a regex with a character class",
    pattern: String.raw`fn\s+handle_`,
  },
  {
    name: "matches case-insensitively",
    pattern: "searchindex",
    caseSensitive: false,
  },
  {
    name: "applies include filters like the plugin",
    pattern: "struct",
    include: "**/*.rs",
    ripgrepArgs: ["--glob", "*.rs"],
  },
  {
    name: "scopes results to the provided path",
    pattern: "SearchIndex",
    path: "src",
  },
  {
    name: "returns no matches when nothing matches",
    pattern: "zzz_nonexistent_pattern_zzz",
  },
  {
    name: "handles regex special characters",
    pattern: String.raw`fn\s+\w+\(`,
  },
  {
    name: "matches pub struct lines",
    pattern: "pub struct",
  },
  {
    name: "skips binary files",
    pattern: "BinarySentinel",
    ripgrepArgs: ["-I"],
  },
  {
    name: "respects gitignored files",
    pattern: "IgnoredSentinel",
  },
];

const globCases: GlobCase[] = [
  {
    name: "matches a simple extension pattern",
    pattern: "**/*.ts",
  },
  {
    name: "matches a nested pattern",
    pattern: "src/**/*.rs",
  },
  {
    name: "matches brace expansion patterns",
    pattern: "**/*.{ts,js}",
  },
  {
    name: "scopes results to the provided path",
    pattern: "**/*.ts",
    path: "src",
  },
  {
    name: "returns an empty list when nothing matches",
    pattern: "**/*.zzz",
  },
];

defineSearchParitySuite("e2e search parity", {
  experimentalSearchIndex: false,
  expectedIndexStatus: "fallback",
});

defineSearchParitySuite("search-parity (indexed)", {
  experimentalSearchIndex: true,
  expectedIndexStatus: "ready",
  indexBuildDelayMs: 500,
});

function defineSearchParitySuite(
  name: string,
  options: {
    experimentalSearchIndex: boolean;
    expectedIndexStatus: string;
    indexBuildDelayMs?: number;
  },
): void {
  maybeDescribe(name, () => {
    let preparedBinary: PreparedBinary = initialBinary;
    const harnesses: E2EHarness[] = [];

    beforeAll(async () => {
      preparedBinary = await prepareBinary();
    });

    afterEach(async () => {
      await cleanupHarnesses(harnesses);
    });

    async function harness(): Promise<E2EHarness> {
      const created = await createHarness(preparedBinary, {
        fixtureNames: [],
        timeoutMs: 10_000,
        tempPrefix: "aft-plugin-search-parity-",
      });
      harnesses.push(created);

      await createFixtureProject(created.tempDir);
      await configureBridge(created, {
        experimentalSearchIndex: options.experimentalSearchIndex,
      });

      if (options.indexBuildDelayMs) {
        await delay(options.indexBuildDelayMs);
      }

      return created;
    }

    for (const grepCase of grepCases) {
      test(`grep parity: ${grepCase.name}`, async () => {
        const h = await harness();

        const response = await h.bridge.send("grep", {
          pattern: grepCase.pattern,
          case_sensitive: grepCase.caseSensitive ?? true,
          include: grepCase.include ? [grepCase.include] : undefined,
          path: grepCase.path,
        });

        expect(response.success).toBe(true);
        expect(String(response.index_status).toLowerCase()).toBe(options.expectedIndexStatus);
        expect(typeof response.text).toBe("string");

        const aftMatches = parseAftMatches(response);
        const rgMatches = ripgrep(grepCase.pattern, h.tempDir, {
          caseSensitive: grepCase.caseSensitive ?? true,
          args: grepCase.ripgrepArgs,
          path: grepCase.path,
        });

        expect(aftMatches).toEqual(rgMatches);
      });
    }

    for (const globCase of globCases) {
      test(`glob parity: ${globCase.name}`, async () => {
        const h = await harness();

        const response = await h.bridge.send("glob", {
          pattern: globCase.pattern,
          path: globCase.path,
        });

        expect(response.success).toBe(true);
        expect(parseAftGlobFiles(response)).toEqual(
          filesystemGlob(globCase.pattern, h.tempDir, globCase.path),
        );
      });
    }
  });
}

async function createFixtureProject(root: string): Promise<void> {
  await mkdir(join(root, "src"), { recursive: true });
  await mkdir(join(root, "docs"), { recursive: true });
  await mkdir(join(root, "scripts"), { recursive: true });
  await mkdir(join(root, "node_modules"), { recursive: true });

  await Promise.all([
    writeFile(join(root, ".gitignore"), ["node_modules/", ""].join("\n"), "utf8"),
    writeFile(join(root, ".fixture-id"), `${root}\n`, "utf8"),
    writeFile(
      join(root, "src", "main.rs"),
      ["pub fn handle_grep() {}", "pub struct SearchIndex;", ""].join("\n"),
      "utf8",
    ),
    writeFile(
      join(root, "src", "lib.rs"),
      ["pub fn handle_glob() {}", "mod search_index;", ""].join("\n"),
      "utf8",
    ),
    writeFile(
      join(root, "src", "utils.ts"),
      ["export function searchIndex() {", '  return "helper";', "}", ""].join("\n"),
      "utf8",
    ),
    writeFile(
      join(root, "src", "utils.test.ts"),
      ['describe("SearchIndex", () => {});', ""].join("\n"),
      "utf8",
    ),
    writeFile(
      join(root, "docs", "guide.md"),
      ["# SearchIndex guide", "SearchIndex reference", ""].join("\n"),
      "utf8",
    ),
    writeFile(
      join(root, "scripts", "helper.ts"),
      ["export const helper = 'SearchIndex';", ""].join("\n"),
      "utf8",
    ),
    writeFile(
      join(root, "node_modules", "dep.ts"),
      ['export const ignored = "IgnoredSentinel";', ""].join("\n"),
      "utf8",
    ),
    writeFile(
      join(root, "src", "binary.bin"),
      Buffer.concat([Buffer.from([0, 255, 0]), Buffer.from("BinarySentinel", "utf8")]),
    ),
  ]);

  runCommand("git", ["init"], root);
  runCommand("git", ["add", "."], root);
  runCommand(
    "git",
    ["-c", "user.name=AFT E2E", "-c", "user.email=aft-e2e@example.com", "commit", "-m", "fixture"],
    root,
    {
      GIT_AUTHOR_NAME: "AFT E2E",
      GIT_AUTHOR_EMAIL: "aft-e2e@example.com",
      GIT_COMMITTER_NAME: "AFT E2E",
      GIT_COMMITTER_EMAIL: "aft-e2e@example.com",
    },
  );
}

async function configureBridge(
  harness: E2EHarness,
  options: { experimentalSearchIndex: boolean },
): Promise<void> {
  const response = await harness.bridge.send("configure", {
    project_root: harness.tempDir,
    harness: "opencode",
    search_index: options.experimentalSearchIndex,
  });

  if (response.success !== true) {
    throw new Error(`configure failed: ${String(response.message ?? response.code ?? "unknown")}`);
  }
}

async function delay(ms: number): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, ms));
}

function ripgrep(
  pattern: string,
  cwd: string,
  options: { caseSensitive?: boolean; args?: string[]; path?: string } = {},
): ParsedGrepMatch[] {
  const args = ["--no-heading", "--line-number", "--color=never"];

  if (options.caseSensitive === false) {
    args.push("-i");
  }

  if (options.args) {
    args.push(...options.args);
  }

  args.push(pattern, options.path ?? ".");

  const result = spawnSync("rg", args, {
    cwd,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });

  if (result.error) {
    throw result.error;
  }

  if (result.status === 1) {
    return [];
  }

  if (result.status !== 0) {
    throw new Error(result.stderr || `rg failed with exit code ${result.status ?? "unknown"}`);
  }

  return result.stdout
    .split("\n")
    .filter(Boolean)
    .map((line) => {
      const parsed = parseRipgrepLine(line);
      // Resolve to absolute path to match AFT's output (which uses canonical absolute paths)
      parsed.file = normalizePath(realpathSync(join(cwd, parsed.file)));
      return parsed;
    })
    .sort(compareGrepMatches);
}

function filesystemGlob(pattern: string, cwd: string, path?: string): string[] {
  const scopedPattern = path ? join(path, pattern) : pattern;
  return globSync(scopedPattern, {
    cwd,
    exclude: [".git/**", "node_modules/**"],
  })
    .map((filePath) => normalizePath(realpathSync(join(cwd, String(filePath)))))
    .sort();
}

function parseAftMatches(response: Record<string, unknown>): ParsedGrepMatch[] {
  const matches = Array.isArray(response.matches)
    ? (response.matches as Array<Record<string, unknown>>)
    : [];

  return matches
    .map((match) => ({
      file: normalizePath(String(match.file ?? "")),
      line: Number(match.line ?? 0),
      text: String(match.line_text ?? match.text ?? "").trim(),
    }))
    .sort(compareGrepMatches);
}

function _parseAftTextMatches(response: Record<string, unknown>): ParsedGrepMatch[] {
  const text = typeof response.text === "string" ? response.text : "";
  const matches: ParsedGrepMatch[] = [];
  let currentFile = "";

  for (const line of text.split("\n")) {
    // New format: file header is just a path (relative), no colon, no decorators
    // Match lines are: "65: text here" (no indentation, no Line prefix)
    const matchLine = line.match(/^(\d+):\s?(.*)$/);
    if (matchLine && currentFile) {
      matches.push({
        file: currentFile,
        line: Number.parseInt(matchLine[1], 10),
        text: matchLine[2].trim(),
      });
      continue;
    }

    // Skip footer lines, empty lines, and "... and N more matches" lines
    if (line.startsWith("Found ") || line.startsWith("... and ") || line.trim() === "") {
      continue;
    }

    // Everything else is a file header — resolve relative to project root
    // The text output uses relative paths within project; resolve to absolute for comparison
    const trimmed = line.trim();
    if (trimmed && !trimmed.includes(": ")) {
      // This is a file path header — resolve to absolute using the project root from response
      const projectRoot = String((response as Record<string, unknown>)._projectRoot ?? "");
      if (projectRoot) {
        currentFile = normalizePath(realpathSync(join(projectRoot, trimmed)));
      } else {
        currentFile = normalizePath(trimmed);
      }
    }
  }

  return matches.sort(compareGrepMatches);
}

function parseAftGlobFiles(response: Record<string, unknown>): string[] {
  const files = Array.isArray(response.files) ? response.files : [];
  return files.map((filePath) => normalizePath(String(filePath))).sort();
}

function parseRipgrepLine(line: string): ParsedGrepMatch {
  const match = line.match(/^(?:\.\/|\.\\)?(.+?):(\d+):(.*)$/);
  if (!match) {
    throw new Error(`Unexpected ripgrep output: ${line}`);
  }

  return {
    file: normalizePath(match[1]),
    line: Number.parseInt(match[2], 10),
    text: match[3].trim(),
  };
}

function compareGrepMatches(left: ParsedGrepMatch, right: ParsedGrepMatch): number {
  return (
    left.file.localeCompare(right.file) ||
    left.line - right.line ||
    left.text.localeCompare(right.text)
  );
}

function normalizePath(filePath: string): string {
  return filePath.replace(/^\.\//, "").replace(/\\/g, "/");
}

function hasCommand(command: string, args: string[]): boolean {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    stdio: ["ignore", "ignore", "ignore"],
  });

  return !result.error && result.status === 0;
}

function runCommand(
  command: string,
  args: string[],
  cwd: string,
  env: Record<string, string> = {},
): void {
  const result = spawnSync(command, args, {
    cwd,
    encoding: "utf8",
    env: { ...process.env, ...env },
    stdio: ["ignore", "pipe", "pipe"],
  });

  if (result.error) {
    throw result.error;
  }

  if (result.status !== 0) {
    throw new Error(result.stderr || result.stdout || `${command} failed`);
  }
}
