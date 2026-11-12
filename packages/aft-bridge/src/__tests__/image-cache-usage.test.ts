import { expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { cacheUsageSnapshot } from "./test-utils/image-cache-usage.js";

function images(run: (observed: string, unseen: string) => void): void {
  const root = mkdtempSync(join(tmpdir(), "aft-image-usage-"));
  try {
    const observed = join(root, "observed");
    const unseen = join(root, "unseen");
    for (const directory of [observed, unseen]) {
      mkdirSync(directory);
      writeFileSync(join(directory, "image"), "fixture");
    }
    run(observed, unseen);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

test("partial process snapshots keep found images and mark unobserved images unknown", () => {
  images((observed, unseen) => {
    for (const paths of [["/proc/202"], ["/proc/202/fd", "/proc/202/maps"]]) {
      const snapshot = cacheUsageSnapshot({
        processPaths: ["/proc/101", "/proc/202"],
        mountinfo: "42 1 0:4 / /proc rw - proc proc rw\n",
        probe: (directory, observer) => {
          if (directory === "/proc/101") {
            observer.path(join(observed, "image"));
            return [];
          }
          return paths.map((path) => ({ path, error: new Error("unreadable process") }));
        },
      });
      expect(snapshot.unreadable).toEqual([{ pid: 202, paths }]);
      expect(snapshot.procfsIncomplete).toBeUndefined();
      expect(snapshot.inUse(observed)).toBe("found");
      expect(snapshot.inUse(unseen)).toBe("unknown");
    }
  });
});

test("hidepid restrictions make unseen images unknown without hiding observed images", () => {
  images((observed, unseen) => {
    for (const [hidepid, expected] of [
      ["", "absent"],
      ["0", "absent"],
      ["off", "absent"],
      ["invisible", "unknown"],
      ["2", "unknown"],
      [null, "unknown"],
    ] as const) {
      const snapshot = cacheUsageSnapshot({
        processPaths: ["/proc/101"],
        mountinfo:
          hidepid === null
            ? null
            : `42 1 0:4 / /proc rw - proc proc rw${hidepid ? `,hidepid=${hidepid}` : ""}\n`,
        probe: (_directory, observer) => {
          observer.path(join(observed, "image"));
          return [];
        },
      });
      expect(snapshot.unreadable).toEqual([]);
      expect(snapshot.inUse(observed)).toBe("found");
      expect(snapshot.inUse(unseen)).toBe(expected);
    }
  });
});
