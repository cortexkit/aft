import { writeFile } from "node:fs/promises";
import { join } from "node:path";

/** Give tool fixtures project evidence without adding source symbols or replacing a build file. */
export async function markProject(root: string): Promise<void> {
  try {
    await writeFile(join(root, "Makefile"), "", { flag: "wx" });
  } catch (error) {
    if (!(error instanceof Error) || !("code" in error) || error.code !== "EEXIST") {
      throw error;
    }
  }
}
