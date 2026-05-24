import { execSync, spawnSync } from "node:child_process";

export function isGhInstalled(): boolean {
  try {
    execSync("gh --version", { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

export function getOpenBrowserCommand(
  url: string,
  platform: NodeJS.Platform = process.platform,
): [string, string[]] {
  return platform === "darwin"
    ? ["open", [url]]
    : platform === "win32"
      ? ["explorer.exe", [url]]
      : ["xdg-open", [url]];
}

export function openBrowser(url: string): void {
  const commands = getOpenBrowserCommand(url);

  try {
    const [cmd, args] = commands as [string, string[]];
    spawnSync(cmd, args, { stdio: "ignore" });
  } catch {
    // no-op — caller can fall back to printing the URL
  }
}

/**
 * Create a GitHub issue via `gh issue create`. Returns the issue URL on
 * success or null on failure.
 *
 * Uses spawnSync with argv array instead of execSync with a shell string —
 * avoids shell metacharacter injection when `title` or `repo` contain
 * backticks, `$(...)`, or `;`. Even though `JSON.stringify` quotes the title,
 * the outer command runs through a shell which reinterprets backticks inside
 * double-quoted strings. spawnSync with shell: false (default) passes argv
 * directly to execve without any shell involvement.
 */
export function createGitHubIssue(
  repo: string,
  title: string,
  body: string,
): { url: string | null; stderr?: string } {
  if (!isGhInstalled()) {
    return { url: null, stderr: "gh CLI not installed" };
  }
  const result = spawnSync(
    "gh",
    ["issue", "create", "--repo", repo, "--title", title, "--body-file", "-"],
    {
      input: body,
      encoding: "utf-8",
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  if (result.error) {
    return { url: null, stderr: result.error.message };
  }
  if (result.status !== 0) {
    return { url: null, stderr: result.stderr?.trim() || `gh exited with status ${result.status}` };
  }
  const url = result.stdout.trim().split(/\r?\n/).pop();
  return { url: url || null };
}
