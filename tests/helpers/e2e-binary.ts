import { accessSync, constants, statSync } from "node:fs";
import { resolve } from "node:path";

/**
 * Whether this environment must declare the staged aft binary for the e2e suites.
 *
 * CI always builds the binary before the Bun suites run, so a missing binary
 * there means setup broke, and the suites must fail instead of skipping into a
 * green run. CI providers spell the flag differently (`CI=true`, `CI=1`), and
 * GitHub Actions also sets `GITHUB_ACTIONS=true`. `AFT_REQUIRE_E2E_BINARY=1`
 * asks for the same strictness outside CI.
 */
export function e2eBinaryRequired(env: Record<string, string | undefined>): boolean {
  const ci = env.CI?.trim().toLowerCase();
  return (
    ci === "true" ||
    ci === "1" ||
    env.GITHUB_ACTIONS === "true" ||
    env.AFT_REQUIRE_E2E_BINARY === "1"
  );
}

export function stagedE2eBinary(env: Record<string, string | undefined>): string | null {
  const explicit = env.AFT_BINARY_PATH?.trim();
  if (explicit) {
    const path = resolve(explicit);
    try {
      const mode = process.platform === "win32" ? constants.F_OK : constants.X_OK;
      accessSync(path, mode);
      if (statSync(path).isFile()) return path;
    } catch {
      // Report the override rather than trying a different binary.
    }
    throw new Error(`e2e setup failed: AFT_BINARY_PATH is not an executable file: ${path}`);
  }
  if (e2eBinaryRequired(env)) {
    throw new Error(
      "e2e setup failed: set AFT_BINARY_PATH to the aft binary staged before Bun tests run",
    );
  }
  return null;
}
