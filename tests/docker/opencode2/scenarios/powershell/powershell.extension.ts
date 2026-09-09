import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import type { HarnessExtension } from "../../harness/types.js";

const here = dirname(fileURLToPath(import.meta.url));

const extension: HarnessExtension = {
  name: "powershell-linux-absence-v1",
  async validate(context) {
    if (context.platform !== "linux") return;
    const scenarios = context.scenarios.filter((scenario) => scenario.tool === "powershell");
    if (scenarios.length !== 0) {
      throw new Error("powershell must not register scenarios on linux");
    }
    const matrix = JSON.parse(await readFile(join(here, "matrix.json"), "utf8")) as {
      rows?: Array<{ trajectories?: Record<string, string> }>;
    };
    const cells = Object.values(matrix.rows?.[0]?.trajectories ?? {});
    if (cells.length !== 7 || cells.some((cell) => cell !== "n/a:platform")) {
      throw new Error("powershell must declare n/a:platform for T1-T7 on linux");
    }
  },
};

export default extension;
