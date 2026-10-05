import { expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import {
  chmodSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { RotatingLogSink } from "../durable-log.js";
import { deliverMigrationNoticeOnce } from "../migration-notices.js";
import { __test__ as __onnxTest__ } from "../onnx-runtime.js";
import { markAnnouncementSeen } from "../paths.js";
import { openPrivateStorageDir, privateMkdirSync } from "../private-storage.js";
import { execTarExtractionSync } from "../tar-executable.js";

test.skipIf(process.platform === "win32")(
  "private_storage_bridge_writers_are_owner_only",
  async () => {
    const scratch = mkdtempSync(join(tmpdir(), "aft-private-bridge-"));
    try {
      const root = join(scratch, "storage");
      markAnnouncementSeen(root, "opencode", "1.0.0");
      deliverMigrationNoticeOnce({
        configPath: join(scratch, "config"),
        digest: "test",
        message: "notice",
        deliver: () => {},
        storePath: join(root, "state", "migration-notices.json"),
      });
      const log = new RotatingLogSink(join(root, "logs", "test.log"));
      log.append("private log\n");
      await log.drain();
      const source = join(scratch, "source");
      mkdirSync(source);
      writeFileSync(join(source, "library.so"), "library");
      chmodSync(join(source, "library.so"), 0o644);
      const archive = join(scratch, "fixture.tar");
      execFileSync("tar", ["-cf", archive, "-C", source, "library.so"]);
      const extracted = join(root, "migration", "extract");
      privateMkdirSync(extracted);
      execTarExtractionSync(["xf", archive, "-C", extracted], 2_000, true);
      __onnxTest__.copyOnnxLibraries(
        {
          libName: "library.so",
          assetName: "fixture",
          archiveType: "tgz",
          librarySha256: "fixture",
        },
        source,
        join(root, "onnxruntime", "fixture"),
        ["library.so"],
        [],
      );
      expect(__onnxTest__.acquireLock(join(root, "onnxruntime", "install.lock"))).toBe(true);
      const paths = [root];
      let checked = 0;
      while (paths.length) {
        const path = paths.pop() as string;
        const info = lstatSync(path);
        expect(info.mode & 0o077, path).toBe(0);
        checked += 1;
        if (info.isDirectory()) paths.push(...readdirSync(path).map((name) => join(path, name)));
      }
      expect(checked).toBeGreaterThan(10);
    } finally {
      rmSync(scratch, { recursive: true, force: true });
    }
  },
);

test.skipIf(process.platform === "win32")(
  "private_storage_bridge_repairs_only_opened_directories",
  () => {
    const scratch = mkdtempSync(join(tmpdir(), "aft-private-bridge-"));
    try {
      const root = join(scratch, "storage");
      const key = join(root, "semantic", "key");
      mkdirSync(key, { recursive: true });
      for (const path of [scratch, root, join(root, "semantic"), key]) chmodSync(path, 0o755);
      writeFileSync(join(key, "old.bin"), "old");
      chmodSync(join(key, "old.bin"), 0o644);
      openPrivateStorageDir(root, key);
      expect(lstatSync(key).mode & 0o777).toBe(0o700);
      expect(lstatSync(scratch).mode & 0o777).toBe(0o755);
      expect(lstatSync(join(key, "old.bin")).mode & 0o777).toBe(0o644);
      const outside = join(scratch, "outside");
      mkdirSync(outside);
      chmodSync(outside, 0o755);
      symlinkSync(outside, join(root, "alias"));
      openPrivateStorageDir(root, join(root, "alias"));
      expect(lstatSync(outside).mode & 0o777).toBe(0o755);
    } finally {
      rmSync(scratch, { recursive: true, force: true });
    }
  },
);
