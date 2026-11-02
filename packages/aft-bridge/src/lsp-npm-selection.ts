import { readFile, realpath, stat } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { lspNpmInstallInvocation } from "./lsp-npm-policy.js";
import { type NpmInvocation, npmCandidates, type ResolvedNpm } from "./npm-resolver.js";

// Pending Ufuk's approval.
export const LSP_NPM_REFUSAL =
  "Cannot use npm at <path> for protected LSP installation (<reason>). Select a genuine npm 7 or newer; pinning a server version does not fix package-manager incompatibility.";

export function lspNpmRefusal(path: string, reason: string): string {
  return LSP_NPM_REFUSAL.replace("<path>", path).replace("<reason>", reason);
}

export type LspNpmSelection =
  | { npm: ResolvedNpm; refusal?: never }
  | { npm?: never; refusal: string };

async function genuineNpm(candidate: ResolvedNpm): Promise<{ npm?: ResolvedNpm; reason: string }> {
  let executable: string;
  try {
    executable = await realpath(candidate.command);
  } catch (error) {
    return { reason: `probe failed: ${String(error)}` };
  }
  // Unix npm resolves to npm's own bin/npm-cli.js. On Windows, bypass the
  // command shim and invoke the adjacent npm package's CLI with its Node.
  const windowsShim = /\.(?:cmd|bat)$/i.test(executable);
  let current = dirname(executable);
  if (windowsShim) current = join(current, "node_modules", "npm");
  for (;;) {
    try {
      const pkg = JSON.parse(await readFile(join(current, "package.json"), "utf8"));
      const bin = typeof pkg.bin === "string" ? pkg.bin : pkg.bin?.npm;
      if (pkg.name === "npm" && typeof bin === "string") {
        const cli = await realpath(resolve(current, bin));
        if (!windowsShim && cli !== executable)
          return { reason: "not npm: command is not npm's declared CLI" };
        const major = /^(\d+)\.\d+\.\d+$/.exec(pkg.version)?.[1];
        if (!major) return { reason: "probe failed: npm manifest has an unknown version" };
        if (Number(major) < 7)
          return { reason: `old npm: ${pkg.version} (requires npm 7 or newer)` };
        if (windowsShim || (process.platform === "win32" && /\.[cm]?js$/i.test(cli))) {
          const node = join(candidate.binDir ?? dirname(executable), "node.exe");
          if (!(await stat(node)).isFile())
            return { reason: "probe failed: adjacent Node is missing" };
          return { npm: { command: node, binDir: dirname(node), cliPath: cli }, reason: "" };
        }
        return { npm: { command: cli, binDir: candidate.binDir }, reason: "" };
      }
    } catch {
      // Keep walking: npm's CLI is below its package root, and PATH aliases
      // may be symlinks into a version manager's installation.
    }
    const parent = dirname(current);
    if (parent === current || windowsShim) break;
    current = parent;
  }
  return { reason: "not npm: no matching npm package manifest and CLI" };
}

/** Verify package identity before either pinned or publication-filtered installs. */
export async function selectLspNpm(
  preferred: ResolvedNpm,
  candidates: readonly ResolvedNpm[] = npmCandidates(),
): Promise<LspNpmSelection> {
  let reason = "not npm";
  const seen = new Set<string>();
  for (const candidate of [preferred, ...candidates]) {
    if (seen.has(candidate.command)) continue;
    seen.add(candidate.command);
    const result = await genuineNpm(candidate);
    if (candidate === preferred) reason = result.reason;
    if (result.npm) return { npm: result.npm };
  }
  return { refusal: lspNpmRefusal(preferred.command, reason) };
}

export async function prepareLspNpmInstall(
  preferred: ResolvedNpm,
  target: string,
  before?: string,
  candidates?: readonly ResolvedNpm[],
): Promise<
  | { npm: ResolvedNpm; invocation: NpmInvocation; refusal?: never }
  | { npm?: never; invocation?: never; refusal: string }
> {
  const selection = await selectLspNpm(preferred, candidates);
  if (!selection.npm) return selection;
  const invocation = await lspNpmInstallInvocation(selection.npm, target, before);
  if (!invocation)
    return {
      refusal: lspNpmRefusal(
        selection.npm.command,
        "probe failed: cannot verify publication-cutoff support",
      ),
    };
  return { npm: selection.npm, invocation };
}
