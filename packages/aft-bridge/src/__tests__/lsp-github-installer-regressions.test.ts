import { describe, expect, spyOn, test } from "bun:test";
import * as childProcess from "node:child_process";
import { createHash } from "node:crypto";
import {
  existsSync,
  linkSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { getAftLspPackagesDir } from "../cache-paths.js";
import { execFileSync } from "../child-process.js";
import { createGithubInstaller } from "../lsp-github-install.js";
import { fetchGithubApiFollowingRedirects, probeGithubReleases } from "../lsp-github-probe.js";
import { findGithubServerById } from "../lsp-github-table.js";
import { withEnv } from "./test-utils/env-guard.js";

type VersionCheck = { last_checked: string; latest_eligible: string | null };

function acquireInstallLock(repo: string) {
  const path = join(getAftLspPackagesDir(), encodeURIComponent(repo), ".aft-installing");
  mkdirSync(dirname(path), { recursive: true });
  try {
    writeFileSync(path, `${process.pid}\n`, { flag: "wx" });
    return { release: () => unlinkSync(path) };
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "EEXIST") return null;
    throw error;
  }
}

/** An installer whose version-check records live in `versionChecks`. */
function installerFixture(versionChecks = new Map<string, VersionCheck>()) {
  return createGithubInstaller({
    log: () => {},
    warn: () => {},
    error: () => {},
    readVersionCheck: (repo) => versionChecks.get(repo) ?? null,
    writeVersionCheck: (repo, latest) => {
      versionChecks.set(repo, { last_checked: new Date().toISOString(), latest_eligible: latest });
    },
    // Cached records stay fresh, so the cached-tag branch is taken when one exists.
    shouldRecheckVersion: (record) => record === null,
    acquireInstallLock,
    withInstallLock: async (repo, action) => {
      const lease = acquireInstallLock(repo);
      if (!lease) return null;
      try {
        return await action();
      } finally {
        lease.release();
      }
    },
    hasRootMarker: () => false,
    relevantExtensionsInProject: () => new Set(),
    reportInstallingBinaries: false,
  });
}

async function cacheFixture(
  fn: (root: string) => Promise<void> | void,
  env: Record<string, string> = {},
): Promise<void> {
  const root = mkdtempSync(join(tmpdir(), "aft-github-install-regression-"));
  try {
    await withEnv({ ...env, AFT_CACHE_DIR: root }, () => fn(root));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

async function withTarPrecheckDiagnostics(operation: () => Promise<void>): Promise<void> {
  const original = childProcess.execFile;
  const started = performance.now();
  const children: Array<{
    command: string;
    child: childProcess.ChildProcess;
    exitObserved: boolean;
    closeObserved: boolean;
    stdoutBytesObserved: number;
    stderrBytesObserved: number;
  }> = [];
  const report = (): void => {
    console.error("Tar precheck child lifecycle:", {
      bunVersion: Bun.version,
      elapsedMs: Math.round(performance.now() - started),
      children: children.map(({ child, ...observed }) => ({
        ...observed,
        pid: child.pid,
        exitCode: child.exitCode,
        signalCode: child.signalCode,
        stdinEnded: child.stdin?.writableEnded,
        stdoutEnded: child.stdout?.readableEnded,
        stderrEnded: child.stderr?.readableEnded,
      })),
    });
  };
  const toolMock = spyOn(childProcess, "execFile").mockImplementation(((
    command: string,
    args: string[],
    options: childProcess.ExecFileOptionsWithStringEncoding,
    callback: (error: childProcess.ExecFileException | null, stdout: string, stderr: string) => void,
  ) => {
    const child = original(command, args, options, callback);
    const observed = {
      command: `${command} ${args[0]}`,
      child,
      exitObserved: false,
      closeObserved: false,
      stdoutBytesObserved: 0,
      stderrBytesObserved: 0,
    };
    children.push(observed);
    child.once("exit", () => {
      observed.exitObserved = true;
    });
    child.once("close", () => {
      observed.closeObserved = true;
    });
    child.stdout?.on("data", (chunk: string | Buffer) => {
      observed.stdoutBytesObserved += Buffer.byteLength(chunk);
    });
    child.stderr?.on("data", (chunk: string | Buffer) => {
      observed.stderrBytesObserved += Buffer.byteLength(chunk);
    });
    return child;
  }) as typeof childProcess.execFile);
  // Log before Bun's unchanged 30 s deadline, even if the pending test is aborted.
  // The listeners observe events after execFile returns; exitCode can also expose
  // a child that exited before those listeners could be attached.
  const diagnostic = setTimeout(report, 29_000);
  try {
    await operation();
  } catch (error) {
    report();
    throw error;
  } finally {
    clearTimeout(diagnostic);
    toolMock.mockRestore();
  }
}

const config = { autoInstall: true, graceDays: 7, versions: {}, disabled: new Set<string>() };
const clangd = findGithubServerById("clangd");
if (!clangd) throw new Error("clangd fixture missing");

describe("release platforms", () => {
  test("clangd declines Linux ARM64 instead of selecting an x64 executable", () => {
    expect(clangd.resolveAsset("linux", "arm64", "21.1.0")).toBeNull();
    expect(clangd.resolveAsset("linux", "x64", "21.1.0")?.name).toBe("clangd-linux-21.1.0.zip");
    expect(clangd.resolveAsset("darwin", "arm64", "21.1.0")?.name).toBe("clangd-mac-21.1.0.zip");
  });
});

describe("archive prechecks", () => {
  test("tar size precheck reads sizes rather than numeric filenames", async () => {
    await withTarPrecheckDiagnostics(() =>
      cacheFixture(async (root) => {
        // A file name that parses as a number larger than the extraction cap.
        const name = "5000000000-payload";
        writeFileSync(join(root, name), "four");
        const archive = join(root, "payload.tar");
        execFileSync("tar", ["-cf", archive, "-C", root, name]);
        expect(await installerFixture()._precheckTarForTesting(archive, "tar")).toBe(4);
      }),
    );
    // Spawns tar twice; a loaded CI runner can exceed bun's 5 s default.
  }, 30_000);
});

describe("cache publication already on main", () => {
  test.skipIf(process.platform === "win32")(
    "executable upgrade leaves the previous executable inode intact",
    async () => {
      await cacheFixture(async (root) => {
        const texlab = findGithubServerById("texlab");
        if (!texlab) throw new Error("texlab fixture missing");
        const oldScript = "#!/bin/sh\n# old\nexit 0\n";
        const newScript = "#!/bin/sh\n# new\nexit 0\n";
        const binary = join(root, "lsp-binaries", "texlab", "bin", "texlab");
        mkdirSync(dirname(binary), { recursive: true });
        writeFileSync(binary, oldScript, { mode: 0o755 });
        writeFileSync(
          join(dirname(binary), "..", ".aft-installed"),
          JSON.stringify({
            version: "0.9.0",
            binarySha256: createHash("sha256").update(oldScript).digest("hex"),
          }),
        );
        // A second name for the old inode: an in-place overwrite would change it.
        const oldLink = join(root, "old-binary");
        linkSync(binary, oldLink);
        const tree = join(root, "tree");
        mkdirSync(tree);
        writeFileSync(join(tree, "texlab"), newScript, { mode: 0o755 });
        const archive = join(root, "texlab.tar.gz");
        execFileSync("tar", ["-czf", archive, "-C", tree, "texlab"]);
        const arch = process.arch === "arm64" ? "arm64" : "x64";
        const platform = process.platform === "darwin" ? "darwin" : "linux";
        const asset = texlab.resolveAsset(platform, arch, "1.0.0");
        if (!asset) throw new Error("texlab asset fixture missing");
        const fakeFetch = (async (url: unknown) =>
          String(url).includes("/releases/tags/")
            ? Response.json({
                tag_name: "1.0.0",
                assets: [
                  {
                    name: asset.name,
                    browser_download_url: `https://github.com/latex-lsp/texlab/releases/download/1.0.0/${asset.name}`,
                  },
                ],
              })
            : new Response(readFileSync(archive))) as typeof fetch;
        const installer = installerFixture();
        const result = installer.runGithubAutoInstall(
          new Set(["texlab"]),
          { ...config, versions: { [texlab.githubRepo]: "1.0.0" } },
          fakeFetch,
        );
        await result.installsComplete;
        await installer.abortInFlightGithubInstalls();
        expect(readFileSync(binary, "utf8")).toBe(newScript);
        expect(readFileSync(oldLink, "utf8")).toBe(oldScript);
        expect(existsSync(join(root, "lsp-binaries", ".quarantine"))).toBe(false);
      });
    },
    // Spawns tar and runs an install; a loaded CI runner can exceed bun's 5 s default.
    30_000,
  );

  test("cache validation cannot quarantine a leased GitHub installation", async () => {
    await cacheFixture(async (root) => {
      const pkg = join(root, "lsp-binaries", "clangd");
      mkdirSync(join(pkg, "bin"), { recursive: true });
      writeFileSync(join(pkg, "bin", "clangd"), "partially published");
      writeFileSync(
        join(pkg, ".aft-installed"),
        JSON.stringify({ version: "1.0.0", binarySha256: "0".repeat(64) }),
      );
      const lease = acquireInstallLock(clangd.githubRepo);
      expect(lease).not.toBeNull();
      const installer = installerFixture();
      try {
        const result = installer.runGithubAutoInstall(
          new Set(),
          { ...config, autoInstall: false },
          fetch,
          { __lockWaitTimeoutMsForTests: 50, __lockPollIntervalMsForTests: 10 },
        );
        expect(result.cachedBinDirs).toEqual([]);
        await result.installsComplete;
        expect(existsSync(pkg)).toBe(true);
        expect(existsSync(join(root, "lsp-binaries", ".quarantine"))).toBe(false);
      } finally {
        lease?.release();
        await installer.abortInFlightGithubInstalls();
      }
    });
  });
});

describe("GitHub install metadata", () => {
  test("github metadata replaces rather than truncates the previous inode", async () => {
    await cacheFixture((root) => {
      const pkg = join(root, "lsp-binaries", "fixture");
      mkdirSync(pkg, { recursive: true });
      const path = join(pkg, ".aft-installed");
      // A second name for the old inode shows whether the writer truncated
      // the file in place (a concurrent reader's view) or renamed over it.
      const old = join(root, "old-metadata");
      writeFileSync(path, "old-metadata");
      linkSync(path, old);
      installerFixture()._writeGithubInstalledMetaForTesting(pkg, "1.0.0", "0".repeat(64));
      expect(readFileSync(old, "utf8")).toBe("old-metadata");
      expect(JSON.parse(readFileSync(path, "utf8")).version).toBe("1.0.0");
      expect(readdirSync(pkg)).toEqual([".aft-installed"]);
    });
  });
});

describe("GitHub release lookups", () => {
  test("live release tags cannot persist unsafe path components", async () => {
    await cacheFixture(async () => {
      const versionChecks = new Map<string, VersionCheck>();
      const fakeFetch = (async () =>
        new Response(
          JSON.stringify([
            {
              tag_name: "../../outside",
              published_at: "2020-01-01T00:00:00Z",
              assets: [],
              draft: false,
              prerelease: false,
            },
          ]),
        )) as typeof fetch;
      const result = await installerFixture(versionChecks)._resolveTargetTagForTesting(
        clangd,
        config,
        fakeFetch,
      );
      expect(result.tag).toBeNull();
      expect(versionChecks.has(clangd.githubRepo)).toBe(false);
    });
  });
  test("tag lookup strips credentials on cross-origin redirects", async () => {
    await cacheFixture(
      async () => {
        const authorizations: Array<string | null> = [];
        const fakeFetch = (async (_url: unknown, init?: RequestInit) => {
          authorizations.push(new Headers(init?.headers).get("authorization"));
          if (authorizations.length === 1)
            return new Response(null, {
              status: 302,
              headers: { location: "https://objects.githubusercontent.com/release.json" },
            });
          return new Response(JSON.stringify({ tag_name: "1.0.0", assets: [] }));
        }) as typeof fetch;
        const release = await installerFixture()._fetchReleaseByTagForTesting(
          "owner/repo",
          "1.0.0",
          fakeFetch,
        );
        expect(release?.tag).toBe("1.0.0");
        expect(authorizations).toEqual(["Bearer fixture-token-not-a-secret", null]);
      },
      { GITHUB_TOKEN: "fixture-token-not-a-secret" },
    );
  });

  for (const branch of ["cached", "live"] as const) {
    test(`${branch} release lookup propagates caller cancellation`, async () => {
      await cacheFixture(async () => {
        const versionChecks = new Map<string, VersionCheck>();
        if (branch === "cached") {
          versionChecks.set(clangd.githubRepo, {
            last_checked: new Date().toISOString(),
            latest_eligible: "1.0.0",
          });
        }
        const controller = new AbortController();
        // The cached branch looks up one tag; the live branch lists releases.
        // Record the signal each kind of request received separately so one
        // branch's signal cannot stand in for the other's.
        const signals = new Map<"tag" | "list", AbortSignal | null>();
        const fakeFetch = (async (url: unknown, init?: RequestInit) => {
          const kind = String(url).includes("/releases/tags/") ? "tag" : "list";
          if (!signals.has(kind)) signals.set(kind, init?.signal ?? null);
          controller.abort();
          return new Response("failure", { status: 500 });
        }) as typeof fetch;
        const result = await installerFixture(versionChecks)._resolveTargetTagForTesting(
          clangd,
          config,
          fakeFetch,
          controller.signal,
        );
        expect(result).toEqual({
          tag: null,
          assets: [],
          blockedByGrace: false,
          reason: "github releases probe failed",
        });
        const observed = signals.get(branch === "cached" ? "tag" : "list");
        expect(observed).toBeInstanceOf(AbortSignal);
        expect(observed?.aborted).toBe(true);
      });
    });
  }
});

describe("GitHub release probe redirects", () => {
  const token = "fixture-token-not-a-secret";
  const releasesUrl = "https://api.github.com/repos/owner/repo/releases?per_page=30";

  /** A fetch that answers the first request with `firstResponse` and later ones with an empty release list. */
  function recordingFetch(firstResponse: () => Response) {
    const requests: Array<{ url: string; authorization: string | null }> = [];
    const fakeFetch = (async (url: unknown, init?: RequestInit) => {
      requests.push({
        url: String(url),
        authorization: new Headers(init?.headers).get("authorization"),
      });
      return requests.length === 1 ? firstResponse() : Response.json([]);
    }) as typeof fetch;
    return { requests, fakeFetch };
  }

  const redirectTo = (location: string) => () =>
    new Response(null, { status: 302, headers: { location } });

  test("probe strips credentials on a cross-origin redirect", async () => {
    await cacheFixture(
      async () => {
        const { requests, fakeFetch } = recordingFetch(
          redirectTo("https://objects.githubusercontent.com/releases.json"),
        );
        const result = await probeGithubReleases("owner/repo", 7, fakeFetch, () => {});
        expect(result).not.toBeNull();
        expect(requests).toEqual([
          { url: releasesUrl, authorization: `Bearer ${token}` },
          { url: "https://objects.githubusercontent.com/releases.json", authorization: null },
        ]);
      },
      { GITHUB_TOKEN: token },
    );
  });

  test("probe keeps credentials on a same-origin redirect", async () => {
    await cacheFixture(
      async () => {
        const moved = "https://api.github.com/repositories/1/releases?per_page=30";
        const { requests, fakeFetch } = recordingFetch(redirectTo(moved));
        const result = await probeGithubReleases("owner/repo", 7, fakeFetch, () => {});
        expect(result).not.toBeNull();
        expect(requests).toEqual([
          { url: releasesUrl, authorization: `Bearer ${token}` },
          { url: moved, authorization: `Bearer ${token}` },
        ]);
      },
      { GITHUB_TOKEN: token },
    );
  });

  test("probe refuses a redirect to a host outside the GitHub allowlist", async () => {
    await cacheFixture(
      async () => {
        const { requests, fakeFetch } = recordingFetch(
          redirectTo("https://evil.example/releases.json"),
        );
        expect(await probeGithubReleases("owner/repo", 7, fakeFetch, () => {})).toBeNull();
        expect(requests.map((request) => request.url)).toEqual([releasesUrl]);
      },
      { GITHUB_TOKEN: token },
    );
  });

  test("a capitalised Authorization header is stripped on a cross-origin redirect", async () => {
    const { requests, fakeFetch } = recordingFetch(
      redirectTo("https://objects.githubusercontent.com/release.json"),
    );
    await fetchGithubApiFollowingRedirects(
      "https://api.github.com/repos/owner/repo/releases/tags/1.0.0",
      { Authorization: `Bearer ${token}` },
      fakeFetch,
    );
    expect(requests.map((request) => request.authorization)).toEqual([`Bearer ${token}`, null]);
  });

  test("redirect following stops after five redirects", async () => {
    let requests = 0;
    const loop = (async () => {
      requests += 1;
      return new Response(null, {
        status: 302,
        headers: { location: "https://api.github.com/repos/owner/repo/releases" },
      });
    }) as unknown as typeof fetch;
    await expect(
      fetchGithubApiFollowingRedirects(
        "https://api.github.com/repos/owner/repo/releases",
        {},
        loop,
      ),
    ).rejects.toThrow("too many redirects");
    expect(requests).toBe(6);
  });
});
