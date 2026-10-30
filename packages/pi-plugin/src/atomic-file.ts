import { randomUUID } from "node:crypto";
import { renameSync, rmSync, writeFileSync } from "node:fs";

/**
 * Publish a complete file by writing a private sibling and renaming it into
 * place. Another host reading the shared LSP cache sees either the previous
 * record or the new one, never a truncated or half-written file.
 */
export function writeAtomicFile(path: string, contents: string): void {
  const temporary = `${path}.tmp-${randomUUID()}`;
  try {
    writeFileSync(temporary, contents, { flag: "wx" });
    renameSync(temporary, path);
  } finally {
    rmSync(temporary, { force: true });
  }
}
