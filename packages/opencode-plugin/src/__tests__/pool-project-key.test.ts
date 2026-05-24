/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { mkdtempSync, realpathSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { BridgePool } from "@cortexkit/aft-bridge";
import { projectRootFor } from "../tools/_shared.js";

const BINARY_PATH = resolve(import.meta.dir, "../../../../target/debug/aft");

/**
 * Pool behavior under the project-keyed design (issue #14).
 *
 * These tests don't actually spawn bridges — they inspect pool bookkeeping
 * using `pool.size` to confirm that two logical sessions pointing at the
 * same project share exactly one bridge, and that two distinct projects
 * get two bridges.
 */
describe("BridgePool project-key sharing", () => {
  test("same canonical project root collapses to one bridge entry", () => {
    const pool = new BridgePool(BINARY_PATH, { timeoutMs: 1_000 }, { harness: "opencode" });
    try {
      // Two different "session IDs" used to key the pool; with the new design
      // only the project root matters, so both calls must hit the same entry.
      const a = pool.getBridge("/tmp/project-one");
      const b = pool.getBridge("/tmp/project-one");
      expect(a).toBe(b);
      expect(pool.size).toBe(1);
    } finally {
      pool.shutdown().catch(() => {});
    }
  });

  test("different project roots get separate bridges", () => {
    const pool = new BridgePool(BINARY_PATH, { timeoutMs: 1_000 }, { harness: "opencode" });
    try {
      pool.getBridge("/tmp/project-one");
      pool.getBridge("/tmp/project-two");
      expect(pool.size).toBe(2);
    } finally {
      pool.shutdown().catch(() => {});
    }
  });

  test("trailing separators normalize to the same key", () => {
    const pool = new BridgePool(BINARY_PATH, { timeoutMs: 1_000 }, { harness: "opencode" });
    try {
      const a = pool.getBridge("/tmp/project-trail");
      const b = pool.getBridge("/tmp/project-trail/");
      const c = pool.getBridge("/tmp/project-trail///");
      expect(a).toBe(b);
      expect(a).toBe(c);
      expect(pool.size).toBe(1);
    } finally {
      pool.shutdown().catch(() => {});
    }
  });

  test("real paths collapse symlinked project roots to one bridge", () => {
    const real = realpathSync(mkdtempSync(join(tmpdir(), "aft-opencode-pool-")));
    const linkDir = realpathSync(mkdtempSync(join(tmpdir(), "aft-opencode-pool-link-")));
    const link = join(linkDir, "link");
    symlinkSync(real, link);

    const pool = new BridgePool(BINARY_PATH, { timeoutMs: 1_000 }, { harness: "opencode" });
    try {
      const viaLink = pool.getBridge(link);
      const viaReal = pool.getBridge(real);

      expect(viaLink).toBe(viaReal);
      expect(pool.size).toBe(1);
    } finally {
      pool.shutdown().catch(() => {});
    }
  });

  test("root-scoped active lookup does not fall back to another project", () => {
    const pool = new BridgePool(BINARY_PATH, { timeoutMs: 1_000 }, { harness: "opencode" });
    try {
      const projectA = pool.getBridge("/tmp/project-a");
      const projectB = pool.getBridge("/tmp/project-b");
      (projectA as { isAlive: () => boolean }).isAlive = () => true;
      (projectB as { isAlive: () => boolean }).isAlive = () => false;

      expect(pool.getActiveBridgeForRoot("/tmp/project-b")).toBeNull();
    } finally {
      pool.shutdown().catch(() => {});
    }
  });
});

describe("projectRootFor", () => {
  test("prefers worktree over directory when both are present", () => {
    const root = projectRootFor({
      directory: "/some/other/subdir",
      worktree: "/tmp",
    });
    // /tmp often resolves to /private/tmp on macOS; accept either canonical form.
    expect([realpathSync("/tmp"), "/tmp"]).toContain(root);
  });

  test("falls back to directory when worktree is missing", () => {
    const root = projectRootFor({ directory: "/tmp" });
    expect([realpathSync("/tmp"), "/tmp"]).toContain(root);
  });

  test("strips trailing separators", () => {
    const root = projectRootFor({ directory: "/tmp/" });
    expect([realpathSync("/tmp"), "/tmp"]).toContain(root);
  });

  test("resolves symlinks so two views of the same dir collapse", () => {
    // Real temp dir, then a symlink to it; both inputs must produce the same
    // canonical root so the pool never spawns two bridges for one project.
    const real = realpathSync(mkdtempSync(join(tmpdir(), "aft-root-")));
    const linkDir = realpathSync(mkdtempSync(join(tmpdir(), "aft-root-link-")));
    const link = join(linkDir, "link");
    symlinkSync(real, link);

    const viaReal = projectRootFor({ directory: real });
    const viaLink = projectRootFor({ directory: link });
    expect(viaLink).toBe(viaReal);
  });
});
