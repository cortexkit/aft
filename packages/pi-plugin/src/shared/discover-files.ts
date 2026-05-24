import { readdir } from "node:fs/promises";
import { extname, join } from "node:path";

/** File extensions that aft_outline supports via tree-sitter or markdown parser */
export const OUTLINE_EXTENSIONS = new Set([
  ".ts",
  ".tsx",
  ".js",
  ".jsx",
  ".mjs",
  ".cjs",
  ".rs",
  ".go",
  ".py",
  ".rb",
  ".c",
  ".cpp",
  ".h",
  ".hpp",
  ".cs",
  ".java",
  ".kt",
  ".scala",
  ".swift",
  ".lua",
  ".ex",
  ".exs",
  ".hs",
  ".sol",
  ".nix",
  ".md",
  ".mdx",
  ".css",
  ".html",
  ".json",
  ".yaml",
  ".yml",
  ".sh",
  ".bash",
  ".zsh",
]);

const SKIP_DIRS = new Set([
  "node_modules",
  ".git",
  "dist",
  "build",
  "out",
  ".next",
  ".nuxt",
  "target",
  "__pycache__",
  ".venv",
  "venv",
  "vendor",
  ".turbo",
  "coverage",
  ".nyc_output",
  ".cache",
]);

/** Recursively discover source files under a directory, skipping common noise directories */
export async function discoverSourceFiles(dir: string, maxFiles = 200): Promise<string[]> {
  const files: string[] = [];

  async function walk(current: string): Promise<void> {
    if (files.length >= maxFiles) return;
    let entries: import("node:fs").Dirent[];
    try {
      entries = await readdir(current, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      if (files.length >= maxFiles) return;
      if (entry.isDirectory()) {
        if (!SKIP_DIRS.has(entry.name) && !entry.name.startsWith(".")) {
          await walk(join(current, entry.name));
        }
      } else if (entry.isFile()) {
        const ext = extname(entry.name).toLowerCase();
        if (OUTLINE_EXTENSIONS.has(ext)) {
          files.push(join(current, entry.name));
        }
      }
    }
  }

  await walk(dir);
  files.sort();
  return files;
}
