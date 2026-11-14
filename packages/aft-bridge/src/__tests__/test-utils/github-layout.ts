import { expect, test } from "bun:test";
import { createHash } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { writeStampedFileDigest } from "../../binary-identity.js";
import { execFileSync, spawn } from "../../child-process.js";
import { acquireEnv } from "./env-guard.js";
import { writeSmokeBinary } from "./smoke-binary.js";

interface Installer {
  __githubBinaryHashCountForTests: () => number;
  abortInFlightGithubInstalls: () => Promise<void>;
  _extractArchiveSafelyForTesting: (
    archive: string,
    destination: string,
    type: "tar.gz",
  ) => Promise<void>;
  runGithubAutoInstall: (
    servers: ReadonlySet<string>,
    config: {
      autoInstall: boolean;
      graceDays: number;
      versions: Readonly<Record<string, string>>;
      disabled: ReadonlySet<string>;
    },
    fetchImpl: typeof fetch,
    options?: {
      onCachedBinDirsChanged?: (directory?: string) => void;
      __lockWaitTimeoutMsForTests?: number;
      __lockPollIntervalMsForTests?: number;
    },
  ) => {
    cachedBinDirs: string[];
    skipped: Array<{ id: string; reason: string }>;
    installsComplete: Promise<void>;
    getCachedBinDirs: () => Promise<string[]>;
  };
}

/**
 * Start a separate process that creates the install lock file at `lock` and
 * holds it until a line arrives on its stdin, so the lock is owned by another
 * live process, as it is when a second plugin instance is installing.
 */
export async function startInstallLockOwner(lock: string) {
  const child = spawn(
    process.execPath,
    [
      "-e",
      `
        const fs = require("node:fs");
        const path = ${JSON.stringify(lock)};
        fs.mkdirSync(require("node:path").dirname(path), { recursive: true });
        fs.writeFileSync(path, process.pid + "\\n" + new Date().toISOString() + "\\nfixture-owner\\n", { flag: "wx" });
        process.stdout.write("locked");
        process.stdin.once("data", () => { fs.unlinkSync(path); process.exit(0); });
      `,
    ],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  const exited = new Promise<void>((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (code) =>
      code === 0 ? resolve() : reject(new Error(`lock owner exited ${code}`)),
    );
  });
  await Promise.race([
    new Promise<void>((resolve) => child.stdout!.once("data", () => resolve())),
    exited.then(() => {
      throw new Error("lock owner exited before readiness");
    }),
  ]);
  return { child, exited };
}

export function githubLayoutTests(installer: Installer): void {
  test("Windows smoke fixture adds the executable suffix only once", () => {
    const root = mkdtempSync(join(tmpdir(), "aft-smoke-suffix-"));
    try {
      const executable = join(root, "texlab.exe");
      expect(writeSmokeBinary(join(root, "texlab"), "win32")).toBe(executable);
      expect(writeSmokeBinary(executable, "win32")).toBe(executable);
      expect(existsSync(executable)).toBe(true);
      expect(existsSync(`${executable}.exe`)).toBe(false);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
  const withoutInstall = {
    autoInstall: false,
    graceDays: 7,
    versions: {},
    disabled: new Set<string>(),
  };
  async function withCachedServer(
    script: string,
    action: (bin: string, root: string) => Promise<void>,
  ) {
    const root = mkdtempSync(join(tmpdir(), "aft-smoke-verdict-"));
    const releaseEnv = await acquireEnv({ AFT_CACHE_DIR: root });
    try {
      const binary = join(root, "lsp-binaries", "texlab", "bin", "texlab");
      mkdirSync(dirname(binary), { recursive: true });
      writeFileSync(binary, script, { mode: 0o755 });
      writeFileSync(
        join(dirname(binary), "..", ".aft-installed"),
        JSON.stringify({
          version: "1.0.0",
          binarySha256: createHash("sha256").update(script).digest("hex"),
        }),
      );
      await action(binary, root);
    } finally {
      releaseEnv();
      rmSync(root, { recursive: true, force: true });
    }
  }
  test.skipIf(process.platform === "win32")(
    "recorded smoke verdict exposes the stamped cache synchronously without spawning",
    async () => {
      const script = '#!/bin/sh\nprintf x >> "${0}.runs"\nexit 0\n';
      await withCachedServer(script, async (binary, root) => {
        const caches = [
          { binary, packageDir: dirname(dirname(binary)), layout: "standalone" },
          {
            binary: join(root, "lsp-binaries", "lua-ls", "extracted", "bin", "lua-language-server"),
            packageDir: join(root, "lsp-binaries", "lua-ls"),
            layout: "distribution",
          },
          {
            binary: join(
              root,
              "lsp-binaries",
              "clangd",
              "extracted",
              "clangd_1.0.0",
              "bin",
              "clangd",
            ),
            packageDir: join(root, "lsp-binaries", "clangd"),
            layout: "distribution",
          },
        ];
        const digest = createHash("sha256").update(script).digest("hex");
        const luaRoot = join(root, "lsp-binaries", "lua-ls", "extracted");
        mkdirSync(luaRoot, { recursive: true });
        writeFileSync(join(luaRoot, "main.lua"), "-- runtime\n");
        mkdirSync(join(luaRoot, "script"));
        mkdirSync(
          join(root, "lsp-binaries", "clangd", "extracted", "clangd_1.0.0", "lib", "clang"),
          { recursive: true },
        );
        for (const cached of caches) {
          mkdirSync(dirname(cached.binary), { recursive: true });
          writeFileSync(cached.binary, script, { mode: 0o755 });
          writeStampedFileDigest(cached.binary, digest);
        }
        for (const verdict of ["passed", "timeout"]) {
          for (const cached of caches) {
            writeFileSync(
              join(cached.packageDir, ".aft-installed"),
              JSON.stringify({
                version: "1.0.0",
                installedAt: "now",
                sha256: digest,
                binarySha256: digest,
                layout: cached.layout,
                smoke: { binarySha256: digest, verdict },
              }),
            );
          }
          const hashes = installer.__githubBinaryHashCountForTests();
          const result = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch);
          for (const cached of caches) {
            expect(result.cachedBinDirs).toContain(dirname(cached.binary));
            expect(existsSync(`${cached.binary}.runs`)).toBe(false);
          }
          await result.installsComplete;
          const refreshed = await result.getCachedBinDirs();
          for (const cached of caches) {
            expect(refreshed).toContain(dirname(cached.binary));
            expect(existsSync(`${cached.binary}.runs`)).toBe(false);
          }
          expect(installer.__githubBinaryHashCountForTests()).toBe(hashes);
        }
      });
    },
  );
  test.skipIf(process.platform === "win32")(
    "cache without a smoke verdict is published only after asynchronous validation",
    async () => {
      const script = '#!/bin/sh\nprintf x >> "${0}.runs"\nexit 0\n';
      await withCachedServer(script, async (binary) => {
        const digest = createHash("sha256").update(script).digest("hex");
        writeStampedFileDigest(binary, digest);
        writeFileSync(
          join(dirname(binary), "..", ".aft-installed"),
          JSON.stringify({
            version: "1.0.0",
            installedAt: "now",
            sha256: digest,
            binarySha256: digest,
            layout: "standalone",
          }),
        );
        // A loaded CI machine can take longer than the 2 s production budget
        // just to start the script; the check must run to completion here.
        const result = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch, {
          __smokeTimeoutMsForTests: 30_000,
        });
        expect(result.cachedBinDirs).not.toContain(dirname(binary));
        await result.installsComplete;
        expect(result.cachedBinDirs).toContain(dirname(binary));
        expect(await result.getCachedBinDirs()).toContain(dirname(binary));
        expect(readFileSync(`${binary}.runs`, "utf8")).toBe("x");
      });
    },
  );
  test.skipIf(process.platform === "win32")(
    "runtime smoke yields to unrelated event-loop work",
    async () => {
      await withCachedServer("#!/bin/sh\nsleep 1\nexit 0\n", async (binary) => {
        let ticks = 0;
        const interval = setInterval(() => {
          ticks += 1;
        }, 10);
        try {
          const result = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch);
          await result.installsComplete;
          expect(ticks).toBeGreaterThan(2);
          expect(await result.getCachedBinDirs()).toContain(dirname(binary));
        } finally {
          clearInterval(interval);
        }
      });
    },
  );
  test.skipIf(process.platform === "win32")(
    "runtime smoke timeout keeps the cache without quarantine or download",
    async () => {
      await withCachedServer("#!/bin/sh\nexec sleep 60\n", async (binary, root) => {
        let downloads = 0;
        const arch = process.arch === "arm64" ? "aarch64" : "x86_64";
        const platform = process.platform === "darwin" ? "macos" : "linux";
        const asset = `texlab-${arch}-${platform}.tar.gz`;
        const fetchImpl = (async (input: string | URL | Request) => {
          if (String(input).includes("/releases/tags/"))
            return Response.json({
              tag_name: "1.0.0",
              assets: [
                {
                  name: asset,
                  browser_download_url: `https://github.com/latex-lsp/texlab/releases/download/1.0.0/${asset}`,
                },
              ],
            });
          downloads += 1;
          throw new Error("a slow smoke must not force a download");
        }) as typeof fetch;
        const result = installer.runGithubAutoInstall(
          new Set(["texlab"]),
          {
            ...withoutInstall,
            autoInstall: true,
            versions: { "latex-lsp/texlab": "1.0.0" },
          },
          fetchImpl,
        );
        await result.installsComplete;
        expect(await result.getCachedBinDirs()).toContain(dirname(binary));
        expect(existsSync(binary)).toBe(true);
        expect(existsSync(join(root, "lsp-binaries", ".quarantine"))).toBe(false);
        expect(downloads).toBe(0);
        const meta = JSON.parse(
          readFileSync(join(dirname(binary), "..", ".aft-installed"), "utf8"),
        );
        expect(meta.smoke).toEqual({ binarySha256: meta.binarySha256, verdict: "timeout" });
      });
    },
  );
  test.skipIf(process.platform === "win32")(
    "unchanged digest reuses the persisted smoke verdict without spawning",
    async () => {
      const script = '#!/bin/sh\nprintf x >> "${0}.runs"\nexit 0\n';
      await withCachedServer(script, async (binary) => {
        const first = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch);
        await first.installsComplete;
        expect(readFileSync(`${binary}.runs`, "utf8")).toBe("x");
        const second = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch);
        await second.installsComplete;
        expect(await second.getCachedBinDirs()).toContain(dirname(binary));
        expect(readFileSync(`${binary}.runs`, "utf8")).toBe("x");
        const metaPath = join(dirname(binary), "..", ".aft-installed");
        const meta = JSON.parse(readFileSync(metaPath, "utf8"));
        expect(meta.smoke).toEqual({ binarySha256: meta.binarySha256, verdict: "passed" });
        const changed = `${script}# changed trusted binary\n`;
        writeFileSync(binary, changed, { mode: 0o755 });
        meta.sha256 = meta.binarySha256 = createHash("sha256").update(changed).digest("hex");
        writeFileSync(metaPath, JSON.stringify(meta));
        const third = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch);
        await third.installsComplete;
        expect(readFileSync(`${binary}.runs`, "utf8")).toBe("xx");
        const migrated = JSON.parse(readFileSync(metaPath, "utf8"));
        delete migrated.smoke;
        writeFileSync(metaPath, JSON.stringify(migrated));
        const missing = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch);
        await missing.installsComplete;
        expect(readFileSync(`${binary}.runs`, "utf8")).toBe("xxx");
      });
    },
  );
  const realArchive = process.env.AFT_TEST_LUALS_ARCHIVE;
  const hasRealArchive = !!realArchive && existsSync(realArchive) && process.platform !== "win32";
  if (!hasRealArchive)
    console.info(
      "SKIP real LuaLS 3.19.1 archive: no offline host archive at AFT_TEST_LUALS_ARCHIVE (POSIX tar.gz required)",
    );
  test.skipIf(!hasRealArchive)(
    "real offline LuaLS 3.19.1 distribution reports its version",
    async () => {
      const root = mkdtempSync(join(tmpdir(), "aft-real-luals-"));
      try {
        await installer._extractArchiveSafelyForTesting(
          realArchive!,
          join(root, "extracted"),
          "tar.gz",
        );
        expect(
          execFileSync(join(root, "extracted", "bin", "lua-language-server"), ["--version"], {
            encoding: "utf8",
            timeout: 2_000,
          }).trim(),
        ).toBe("3.19.1");
      } finally {
        rmSync(root, { recursive: true, force: true });
      }
    },
  );
  test.skipIf(process.platform === "win32")(
    "cached standalone server rejects definite runtime smoke failure",
    async () => {
      const root = mkdtempSync(join(tmpdir(), "aft-smoke-cache-"));
      const releaseEnv = await acquireEnv({ AFT_CACHE_DIR: root });
      try {
        const binDir = join(root, "lsp-binaries", "texlab", "bin");
        for (const script of ["#!/bin/sh\nexit 1\n"]) {
          mkdirSync(binDir, { recursive: true });
          writeFileSync(join(binDir, "texlab"), script, { mode: 0o755 });
          writeFileSync(
            join(dirname(binDir), ".aft-installed"),
            JSON.stringify({
              version: "1.0.0",
              binarySha256: createHash("sha256").update(script).digest("hex"),
            }),
          );
          const started = Date.now();
          const result = installer.runGithubAutoInstall(
            new Set(),
            { autoInstall: false, graceDays: 7, versions: {}, disabled: new Set() },
            fetch,
          );
          await result.installsComplete;
          expect(await result.getCachedBinDirs()).not.toContain(binDir);
          expect(Date.now() - started).toBeLessThan(4_000);
        }
      } finally {
        releaseEnv();
        rmSync(root, { recursive: true, force: true });
      }
    },
  );
  test.skipIf(process.platform === "win32")(
    "busy GitHub cache validation retries after lock release without spawning",
    async () => {
      const script = '#!/bin/sh\nprintf x >> "${0}.runs"\nexit 0\n';
      await withCachedServer(script, async (binary, root) => {
        const first = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch);
        await first.installsComplete;
        rmSync(`${binary}.runs`, { force: true });
        const luaBinDir = join(root, "lsp-binaries", "lua-ls", "extracted", "bin");
        const luaBinary = join(luaBinDir, "lua-language-server");
        const luaScript = '#!/bin/sh\nprintf x >> "${0}.runs"\nexit 0\n';
        const luaDigest = createHash("sha256").update(luaScript).digest("hex");
        mkdirSync(luaBinDir, { recursive: true });
        writeFileSync(luaBinary, luaScript, { mode: 0o755 });
        writeStampedFileDigest(luaBinary, luaDigest);
        mkdirSync(join(dirname(luaBinDir), "script"));
        writeFileSync(join(dirname(luaBinDir), "main.lua"), "-- runtime\n");
        writeFileSync(
          join(root, "lsp-binaries", "lua-ls", ".aft-installed"),
          JSON.stringify({
            version: "3.19.1",
            installedAt: "now",
            sha256: luaDigest,
            binarySha256: luaDigest,
            layout: "distribution",
          }),
        );
        const lock = join(
          root,
          "lsp-packages",
          encodeURIComponent("latex-lsp/texlab"),
          ".aft-installing",
        );
        const { child, exited } = await startInstallLockOwner(lock);
        try {
          let otherServerReady!: () => void;
          const otherServerReadyPromise = new Promise<void>((resolve) => {
            otherServerReady = resolve;
          });
          const held = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch, {
            onCachedBinDirsChanged: (directory) => {
              if (directory === luaBinDir) otherServerReady();
            },
          });
          expect(held.cachedBinDirs).not.toContain(dirname(binary));
          let completed = false;
          const installsComplete = held.installsComplete.then(() => {
            completed = true;
          });
          let timeout: ReturnType<typeof setTimeout>;
          const notifyTimeout = new Promise<void>((_, reject) => {
            timeout = setTimeout(() => reject(new Error("LuaLS did not validate")), 3_000);
          });
          try {
            await Promise.race([otherServerReadyPromise, notifyTimeout]);
          } finally {
            clearTimeout(timeout!);
          }
          expect(held.cachedBinDirs).toContain(luaBinDir);
          expect(completed).toBe(false);
          const hashes = installer.__githubBinaryHashCountForTests();
          child.stdin!.end("release");
          await exited;
          await installsComplete;
          expect(held.cachedBinDirs).toContain(dirname(binary));
          expect(await held.getCachedBinDirs()).toContain(dirname(binary));
          expect(existsSync(`${binary}.runs`)).toBe(false);
          expect(installer.__githubBinaryHashCountForTests()).toBe(hashes);
          expect(existsSync(join(root, "lsp-binaries", ".quarantine"))).toBe(false);
        } finally {
          if (child.exitCode === null) child.stdin!.end("release");
          await exited.catch(() => {});
        }
      });
    },
  );
  test.skipIf(process.platform === "win32")(
    "busy GitHub cache validation publishes an upgrade after the lock is released",
    async () => {
      const script = "#!/bin/sh\nexit 0\n";
      await withCachedServer(script, async (binary, root) => {
        const first = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch);
        await first.installsComplete;
        const lock = join(
          root,
          "lsp-packages",
          encodeURIComponent("latex-lsp/texlab"),
          ".aft-installing",
        );
        const { child, exited } = await startInstallLockOwner(lock);
        try {
          const held = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch);
          expect(held.cachedBinDirs).not.toContain(dirname(binary));

          const upgraded = `${script}# upgraded while locked\n`;
          const digest = createHash("sha256").update(upgraded).digest("hex");
          writeFileSync(binary, upgraded, { mode: 0o755 });
          writeStampedFileDigest(binary, digest);
          writeFileSync(
            join(dirname(binary), "..", ".aft-installed"),
            JSON.stringify({
              version: "2.0.0",
              installedAt: "now",
              sha256: digest,
              binarySha256: digest,
              layout: "standalone",
              smoke: { binarySha256: digest, verdict: "passed" },
            }),
          );

          child.stdin!.end("release");
          await exited;
          await held.installsComplete;
          expect(held.cachedBinDirs).toContain(dirname(binary));
          expect(await held.getCachedBinDirs()).toContain(dirname(binary));
          expect(
            JSON.parse(readFileSync(join(dirname(binary), "..", ".aft-installed"), "utf8")),
          ).toMatchObject({ version: "2.0.0", binarySha256: digest });
          expect(existsSync(join(root, "lsp-binaries", ".quarantine"))).toBe(false);
        } finally {
          if (child.exitCode === null) child.stdin!.end("release");
          await exited.catch(() => {});
        }
      });
    },
  );
  test.skipIf(process.platform === "win32")(
    "busy GitHub cache validation reports a bounded lock wait timeout",
    async () => {
      const script = "#!/bin/sh\nexit 0\n";
      await withCachedServer(script, async (binary, root) => {
        const lock = join(
          root,
          "lsp-packages",
          encodeURIComponent("latex-lsp/texlab"),
          ".aft-installing",
        );
        const { child, exited } = await startInstallLockOwner(lock);
        try {
          const started = Date.now();
          const timedOut = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch, {
            __lockWaitTimeoutMsForTests: 500,
            __lockPollIntervalMsForTests: 10,
          });
          expect(Date.now() - started).toBeLessThan(200);
          expect(timedOut.cachedBinDirs).not.toContain(dirname(binary));
          await timedOut.installsComplete;
          expect(timedOut.skipped).toContainEqual({
            id: "texlab",
            reason: "install lock held by another process for 10 minutes",
          });
          expect(existsSync(join(root, "lsp-binaries", ".quarantine"))).toBe(false);
        } finally {
          if (child.exitCode === null) child.stdin!.end("release");
          await exited.catch(() => {});
        }
      });
    },
  );
  test.skipIf(process.platform === "win32")(
    "shutdown abort ends a busy GitHub cache wait while the lock is still held",
    async () => {
      const script = "#!/bin/sh\nexit 0\n";
      await withCachedServer(script, async (binary, root) => {
        const lock = join(
          root,
          "lsp-packages",
          encodeURIComponent("latex-lsp/texlab"),
          ".aft-installing",
        );
        const { child, exited } = await startInstallLockOwner(lock);
        try {
          // Both a server that would install and one that only validates its
          // cache must stop waiting when shutdown aborts in-flight work.
          const configs = [
            { servers: new Set<string>(), config: withoutInstall },
            {
              servers: new Set(["texlab"]),
              config: {
                ...withoutInstall,
                autoInstall: true,
                versions: { "latex-lsp/texlab": "1.0.0" },
              },
            },
          ];
          for (const { servers, config } of configs) {
            let fetches = 0;
            const fetchImpl = (async () => {
              fetches += 1;
              throw new Error("an aborted cache wait must not start an install");
            }) as unknown as typeof fetch;
            // The default ten-minute lock wait stays in force: only the abort
            // can end this wait within the bound below.
            const held = installer.runGithubAutoInstall(servers, config, fetchImpl);
            expect(held.cachedBinDirs).not.toContain(dirname(binary));
            const started = Date.now();
            let timeout: ReturnType<typeof setTimeout>;
            const bound = new Promise<never>((_, reject) => {
              timeout = setTimeout(
                () => reject(new Error("installsComplete did not settle after abort")),
                2_000,
              );
            });
            try {
              // abortInFlightGithubInstalls also awaits the aborted work, so it
              // sits inside the bound too.
              await Promise.race([
                Promise.all([installer.abortInFlightGithubInstalls(), held.installsComplete]),
                bound,
              ]);
            } finally {
              clearTimeout(timeout!);
            }
            expect(Date.now() - started).toBeLessThan(2_000);
            expect(child.exitCode).toBeNull();
            expect(existsSync(lock)).toBe(true);
            expect(held.cachedBinDirs).not.toContain(dirname(binary));
            expect(held.skipped).not.toContainEqual({
              id: "texlab",
              reason: "install lock held by another process for 10 minutes",
            });
            expect(fetches).toBe(0);
          }
        } finally {
          if (child.exitCode === null) child.stdin!.end("release");
          await exited.catch(() => {});
        }
      });
    },
  );
  test.skipIf(process.platform === "win32")(
    "failed distribution upgrade preserves the old executable and runtime",
    async () => {
      const root = mkdtempSync(join(tmpdir(), "aft-upgrade-layout-"));
      const releaseEnv = await acquireEnv({ AFT_CACHE_DIR: root });
      try {
        const source = join(root, "fixture");
        mkdirSync(join(source, "bin"), { recursive: true });
        mkdirSync(join(source, "script"));
        const script = '#!/bin/sh\ncat "$(dirname "$0")/../main.lua"\n';
        const sourceBinary = join(source, "bin", "lua-language-server");
        writeFileSync(sourceBinary, script, { mode: 0o755 });
        writeFileSync(join(source, "main.lua"), "working old runtime\n");
        const archive = join(root, "fixture.tar.gz");
        const pack = () =>
          execFileSync("tar", ["-czf", archive, "-C", source, "bin", "main.lua", "script"]);
        pack();
        const fetchImpl = (async (input: string | URL | Request) => {
          if (String(input).includes("/releases/tags/")) {
            const version = String(input).split("/").pop();
            const asset = `lua-language-server-${version}-${process.platform}-${process.arch}.tar.gz`;
            return Response.json({
              tag_name: version,
              assets: [
                {
                  name: asset,
                  browser_download_url: `https://github.com/LuaLS/lua-language-server/releases/download/${version}/${asset}`,
                },
              ],
            });
          }
          return new Response(readFileSync(archive));
        }) as typeof fetch;
        const install = (version: string) =>
          installer.runGithubAutoInstall(
            new Set(["lua-ls"]),
            {
              ...withoutInstall,
              autoInstall: true,
              versions: { "LuaLS/lua-language-server": version },
              // The failed-smoke case needs the staged smoke to finish, not
              // time out on a loaded machine and keep the bad install.
              __smokeTimeoutMsForTests: 30_000,
            },
            fetchImpl,
          );
        const first = install("3.19.1");
        await first.installsComplete;
        const pkg = join(root, "lsp-binaries", "lua-ls");
        const executable = join(pkg, "extracted", "bin", "lua-language-server");
        expect(execFileSync(executable, ["--version"], { encoding: "utf8" }).trim()).toBe(
          "working old runtime",
        );
        const meta = readFileSync(join(pkg, ".aft-installed"), "utf8");

        for (const failure of ["missing executable", "failed smoke"]) {
          writeFileSync(join(source, "main.lua"), "replacement runtime\n");
          if (failure === "missing executable") rmSync(sourceBinary);
          else writeFileSync(sourceBinary, "#!/bin/sh\nexit 1\n", { mode: 0o755 });
          pack();
          const upgrade = install(failure === "missing executable" ? "3.19.2" : "3.19.3");
          await upgrade.installsComplete;
          expect(existsSync(join(pkg, ".aft-install-failed.json"))).toBe(true);
          expect(readFileSync(join(pkg, ".aft-install-failed.json"), "utf8")).toContain(
            failure === "missing executable"
              ? "extracted binary not found"
              : "failed runtime smoke",
          );
          expect(existsSync(executable)).toBe(true);
          expect(execFileSync(executable, ["--version"], { encoding: "utf8" }).trim()).toBe(
            "working old runtime",
          );
          expect(readFileSync(join(pkg, ".aft-installed"), "utf8")).toBe(meta);
          expect(await upgrade.getCachedBinDirs()).toContain(dirname(executable));
          expect(
            readdirSync(pkg).filter(
              (name) => name.includes(".staging-") || name.includes(".previous-"),
            ),
          ).toEqual([]);
        }
      } finally {
        releaseEnv();
        rmSync(root, { recursive: true, force: true });
      }
    },
  );
  test.skipIf(process.platform === "win32")(
    "LuaLS distribution runs and repairs a legacy copy-out cache",
    async () => {
      const root = mkdtempSync(join(tmpdir(), "aft-luals-layout-"));
      const releaseEnv = await acquireEnv({ AFT_CACHE_DIR: root });
      try {
        const source = join(root, "fixture", "bin");
        mkdirSync(source, { recursive: true });
        const script = '#!/bin/sh\ncat "$(dirname "$0")/../main.lua"\n';
        writeFileSync(join(source, "lua-language-server"), script, { mode: 0o755 });
        writeFileSync(join(dirname(source), "main.lua"), "3.19.1\n");
        mkdirSync(join(dirname(source), "script"));
        const archive = join(root, "fixture.tar.gz");
        execFileSync("tar", ["-czf", archive, "-C", dirname(source), "bin", "main.lua", "script"]);
        const asset = `lua-language-server-3.19.1-${process.platform}-${process.arch}.tar.gz`;
        let downloads = 0;
        const fetchImpl = (async (input: string | URL | Request) => {
          if (String(input).includes("/releases/tags/")) {
            return Response.json({
              tag_name: "3.19.1",
              assets: [
                {
                  name: asset,
                  browser_download_url: `https://github.com/LuaLS/lua-language-server/releases/download/3.19.1/${asset}`,
                },
              ],
            });
          }
          downloads += 1;
          return new Response(readFileSync(archive));
        }) as typeof fetch;
        const config = {
          autoInstall: true,
          graceDays: 7,
          versions: { "LuaLS/lua-language-server": "3.19.1" },
          disabled: new Set<string>(),
        };
        const pkg = join(root, "lsp-binaries", "lua-ls");
        const oldBin = join(pkg, "bin", "lua-language-server");
        mkdirSync(dirname(oldBin), { recursive: true });
        writeFileSync(oldBin, script, { mode: 0o755 });
        const digest = createHash("sha256").update(script).digest("hex");
        writeFileSync(
          join(pkg, ".aft-installed"),
          JSON.stringify({
            version: "3.19.1",
            sha256: digest,
            binarySha256: digest,
          }),
        );

        const result = installer.runGithubAutoInstall(new Set(["lua-ls"]), config, fetchImpl);
        expect(result.cachedBinDirs).not.toContain(dirname(oldBin));
        await result.installsComplete;
        const realBin = join(pkg, "extracted", "bin");
        expect(await result.getCachedBinDirs()).toContain(realBin);
        expect(
          execFileSync(join(realBin, "lua-language-server"), ["--version"], {
            encoding: "utf8",
          }).trim(),
        ).toBe("3.19.1");
        expect(existsSync(oldBin)).toBe(false);
        expect(downloads).toBe(1);
        const cached = installer.runGithubAutoInstall(new Set(["lua-ls"]), config, fetchImpl);
        await cached.installsComplete;
        expect(cached.cachedBinDirs).toContain(realBin);
        expect(downloads).toBe(1);

        rmSync(join(realBin, "..", "main.lua"));
        const damaged = installer.runGithubAutoInstall(new Set(["lua-ls"]), config, fetchImpl);
        expect(damaged.cachedBinDirs).not.toContain(realBin);
        await damaged.installsComplete;
        expect(await damaged.getCachedBinDirs()).toContain(realBin);
        expect(downloads).toBe(2);
        rmSync(join(realBin, "..", "script"), { recursive: true });
        const missingScript = installer.runGithubAutoInstall(
          new Set(["lua-ls"]),
          config,
          fetchImpl,
        );
        expect(missingScript.cachedBinDirs).not.toContain(realBin);
        await missingScript.installsComplete;
        expect(await missingScript.getCachedBinDirs()).toContain(realBin);
        expect(downloads).toBe(3);
      } finally {
        releaseEnv();
        rmSync(root, { recursive: true, force: true });
      }
    },
  );
}
