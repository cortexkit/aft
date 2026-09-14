import { appendFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const [bundlePath, resultPath] = process.argv.slice(2);
if (!bundlePath || !resultPath) throw new Error("bundle and result paths are required");

const cache = await import(pathToFileURL(bundlePath).href);
const lease = cache.claimLspAutoInstallPass();
appendFileSync(resultPath, lease ? "winner\n" : "skipped\n");
if (lease) {
  await new Promise((resolve) => setTimeout(resolve, 100));
  lease.release();
}
process.exit(0);
