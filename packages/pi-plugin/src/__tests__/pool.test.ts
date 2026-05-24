/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { mkdtempSync, realpathSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { BridgePool } from "@cortexkit/aft-bridge";

describe("Pi BridgePool", () => {
  test("getActiveBridgeForRoot returns the bridge when it is alive for the requested root", () => {
    const pool = new BridgePool("/tmp/aft", { timeoutMs: 1_000 }, { harness: "pi" });
    try {
      const projectA = pool.getBridge("/tmp/project-a");
      const projectB = pool.getBridge("/tmp/project-b");
      (projectA as any).isAlive = () => true;
      (projectB as any).isAlive = () => true;

      // Returns the correct bridge when alive for the root
      expect(pool.getActiveBridgeForRoot("/tmp/project-b")).toBe(projectB);
      expect(pool.getActiveBridgeForRoot("/tmp/project-a")).toBe(projectA);
    } finally {
      pool.shutdown().catch(() => {});
    }
  });

  test("getActiveBridgeForRoot returns null when bridge for root is not alive", () => {
    const pool = new BridgePool("/tmp/aft", { timeoutMs: 1_000 }, { harness: "pi" });
    try {
      const projectA = pool.getBridge("/tmp/project-a");
      const projectB = pool.getBridge("/tmp/project-b");
      (projectA as any).isAlive = () => true;
      (projectB as any).isAlive = () => false;

      // Does NOT fall back to projectA — returns null when projectB is dead
      expect(pool.getActiveBridgeForRoot("/tmp/project-b")).toBeNull();
      expect(pool.getActiveBridgeForRoot("/tmp/project-a")).toBe(projectA);
    } finally {
      pool.shutdown().catch(() => {});
    }
  });

  test("root-scoped active lookup does not fall back to another project", () => {
    const pool = new BridgePool("/tmp/aft", { timeoutMs: 1_000 }, { harness: "pi" });
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

  test("trailing slash and backslash normalize to the same key", () => {
    const pool = new BridgePool("/tmp/aft", { timeoutMs: 1_000 }, { harness: "pi" });
    try {
      const a = pool.getBridge("C:\\repo\\");
      const b = pool.getBridge("C:\\repo");
      const c = pool.getBridge("/tmp/project/");
      const d = pool.getBridge("/tmp/project");

      expect(a).toBe(b);
      expect(c).toBe(d);
      expect(pool.size).toBe(2);
    } finally {
      pool.shutdown().catch(() => {});
    }
  });

  test("canonical paths collapse trailing slash variants to one bridge", () => {
    const real = realpathSync(mkdtempSync(join(tmpdir(), "aft-pi-pool-")));
    const linkDir = realpathSync(mkdtempSync(join(tmpdir(), "aft-pi-pool-link-")));
    const link = join(linkDir, "link");
    symlinkSync(real, link);

    const pool = new BridgePool("/tmp/aft", { timeoutMs: 1_000 }, { harness: "pi" });
    try {
      const a = pool.getBridge(link);
      const b = pool.getBridge(`${link}/`);

      expect(a).toBe(b);
      expect(pool.size).toBe(1);
    } finally {
      pool.shutdown().catch(() => {});
    }
  });
});
