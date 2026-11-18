import { expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execTarExtraction, windowsTarExecutable } from "../tar-executable.js";

test("private tar extraction yields and preserves private member permissions", async () => {
  const dir = mkdtempSync(join(tmpdir(), "aft-tar-async-"));
  try {
    const file = join(dir, "library.txt");
    const archive = join(dir, "ort.tar.gz");
    writeFileSync(file, "library bytes", { mode: 0o644 });
    execFileSync(windowsTarExecutable(), ["czf", archive, "-C", dir, "library.txt"]);
    rmSync(file);
    const extraction = execTarExtraction(["xzf", archive, "-C", dir], 120_000, true);
    let settled = false;
    void extraction.then(() => {
      settled = true;
    });
    for (let i = 0; i < 10; i++) await Promise.resolve();
    expect(settled).toBe(false);
    await extraction;
    expect(readFileSync(file, "utf8")).toBe("library bytes");
    if (process.platform !== "win32") expect(statSync(file).mode & 0o077).toBe(0);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
