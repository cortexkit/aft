import { describe, expect, test } from "bun:test";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const testDir = dirname(fileURLToPath(import.meta.url));
const pluginTestRoot = resolve(testDir, "..");

interface AcceptanceRow {
  sliceId: `S${number}${string}`;
  claim: string;
  governingSource: string;
  testFile: string;
  testPattern: string;
}

const acceptanceMatrix: AcceptanceRow[] = [
  {
    sliceId: "S1",
    claim: "Modern V1 host loads ./server and ./tui while ignoring effect and setup keys",
    governingSource: "pack §13.1, rulings R2, R5, R6, R19, R20; OC record pm_7ceb3a96",
    testFile: "load-matrix/load-matrix.ts",
    testPattern: "modern V1 host selects ./server and ignores effect and setup",
  },
  {
    sliceId: "S1",
    claim: "Function-default entry is rejected by the V2 host loader before setup",
    governingSource: "pack §13.1, ruling R19; OC record pm_7ceb3a96",
    testFile: "load-matrix/load-matrix.ts",
    testPattern: "function-default entry is rejected by the V2 host loader before setup",
  },
  {
    sliceId: "S2",
    claim: "V2 host loader selects effect entrypoint before setup and runs it once",
    governingSource: "delta §6, rulings R1, R5; OC record pm_4a4e8133, pm_7725ea5f",
    testFile: "entry/server-effect.test.ts",
    testPattern: "effect",
  },
  {
    sliceId: "S3",
    claim:
      "Two V2 Locations share one daemon and reload without leaking watchers, ports, or processes",
    governingSource: "rulings R16, R21; OC record pm_7725ea5f",
    testFile: "load-matrix/load-matrix.ts",
    testPattern:
      "enabled V2 loader shares one daemon across two Locations and reloads without leaks",
  },
  {
    sliceId: "S4",
    claim: "All AFT tools advertise options.codemode: false and match V1 projected definitions",
    governingSource: "pack §4, §5, delta §3a, rulings R4, R17",
    testFile: "tool-surface/v2-tool-surface.test.ts",
    testPattern: "codemode: false",
  },
  {
    sliceId: "S5",
    claim:
      "V2 hoisted filesystem mutations route through host permission.create with path headers and PatchDiff",
    governingSource:
      "delta §1, rulings R1, R3, R4, R15; OC record pm_4a4e8133, pm_7ceb3a96, pm_7725ea5f",
    testFile: "permissions/v2-permission.test.ts",
    testPattern: "requestPermission",
  },
  {
    sliceId: "S5",
    claim: "Closed ask-site inventory R4 covers all permission sites on V2",
    governingSource: "ruling R4; OC record pm_4a4e8133",
    testFile: "permissions/ask-site-inventory.test.ts",
    testPattern: "ask",
  },
  {
    sliceId: "S6",
    claim:
      "Host interruption triggers Effect fiber cancellation, terminates foreground process, and records call_aborted",
    governingSource: "delta §2, rulings R1, R11, R22; OC record pm_4a4e8133, pm_7725ea5f",
    testFile: "cancellation/effect-cancellation.test.ts",
    testPattern: "call_aborted",
  },
  {
    sliceId: "S6",
    claim:
      "Background completion wakes use session.prompt with steer delivery and synthetic status-only updates",
    governingSource: "delta §2, rulings R1, R7; OC record pm_4a4e8133",
    testFile: "wakes/session-delivery.test.ts",
    testPattern: "steer",
  },
  {
    sliceId: "S7",
    claim:
      "Typed AftRpc registers with supervisor and emits statusInvalidated, showStatusDialog, indexProgress",
    governingSource: "delta §3b, §3e, rulings R1, R8, R9, R10; OC record pm_4a4e8133, pm_7725ea5f",
    testFile: "rpc/register.test.ts",
    testPattern: "AftRpc",
  },
  {
    sliceId: "S8",
    claim:
      "V2 setup and doctor detect host generation, singular plugin key, and exact version pins",
    governingSource: "delta §4, ruling R12; OC record pm_7ceb3a96",
    testFile: "tui/v2-setup.test.tsx",
    testPattern: "setupV2Tui",
  },
  {
    sliceId: "S9",
    claim:
      "GA pin move to @opencode/*@2.0.3 is verified against unpacked package contracts while retaining the beta audit",
    governingSource:
      "constraints §Coexistence testing; GA delta audit oc2-ga-2.0.3; prior OC record pm_7725ea5f",
    testFile: "matrix/acceptance-matrix.test.ts",
    testPattern: "oc2-ga-2.0.3",
  },
  {
    sliceId: "S10",
    claim:
      "GA pin move to @opencode/*@2.0.11 re-derives every relied-on contract point from the unpacked dist, and records both that the client package declares permission.create and that the server plugin context is never handed that client",
    governingSource: "GA delta audit oc2-ga-2.0.11",
    testFile: "matrix/acceptance-matrix.test.ts",
    testPattern: "oc2-ga-2.0.11",
  },
];

describe("OpenCode V2 delta audit evidence", () => {
  const betaAuditFile = join(testDir, "delta-audit-beta-19234.md");
  const gaAuditFile = join(testDir, "delta-audit-ga-2.0.3.md");
  const currentAuditFile = join(testDir, "delta-audit-ga-2.0.11.md");

  test("beta evidence remains on disk", () => {
    expect(existsSync(betaAuditFile)).toBe(true);
    const content = readFileSync(betaAuditFile, "utf8");
    expect(content).toContain("0.0.0-beta-19234");
    expect(content).toContain("pm_7725ea5f");
    expect(content).toContain("ZERO delta");
  });

  test("GA evidence records the exact 2.0.3 pin and source-derived record", () => {
    expect(existsSync(gaAuditFile)).toBe(true);
    const content = readFileSync(gaAuditFile, "utf8");
    expect(content).toContain("@opencode/*@2.0.3");
    expect(content).toContain("oc2-ga-2.0.3");
    expect(content).toContain("2026-09-12T23:46:23.757Z");
    expect(content).toContain("expected_fail:upstream#37164");
  });

  test("GA evidence diffs all nine relied-on contract lines with dist citations", () => {
    const content = readFileSync(gaAuditFile, "utf8");
    for (let line = 1; line <= 9; line += 1) {
      expect(content).toContain(`${line}. **`);
    }
    expect(content).toContain("dist/chunks/mime-771dt0vh.js");
    expect(content).toContain("dist/effect/plugin.d.ts");
    expect(content).toContain("dist/host.js");
  });

  test("current GA evidence records the exact 2.0.11 pin and source-derived record", () => {
    expect(existsSync(currentAuditFile)).toBe(true);
    const content = readFileSync(currentAuditFile, "utf8");
    expect(content).toContain("@opencode/*@2.0.11");
    expect(content).toContain("oc2-ga-2.0.11");
    expect(content).toContain("2026-09-20T08:57:16.764Z");
  });

  test("current GA evidence diffs every relied-on contract line with dist citations", () => {
    const content = readFileSync(currentAuditFile, "utf8");
    for (let line = 1; line <= 15; line += 1) {
      expect(content).toContain(`${line}. **`);
    }
    expect(content).toContain("@opencode/client@2.0.11/dist/promise/client.d.ts");
    expect(content).toContain("@opencode/plugin@2.0.11/dist/effect/plugin.d.ts");
    expect(content).toContain("@opencode/plugin@2.0.11/dist/host.js");
    expect(content).toContain("@opencode/core@2.0.11/dist/chunks/");
  });

  test("the current audit separates the client declaration from who receives it", () => {
    const content = readFileSync(currentAuditFile, "utf8");
    // The audit records two independent facts about permissions: that the
    // @opencode/client package declares permission.create, and that the server
    // plugin context is never handed that client. Keep both, so the package
    // declaration is never mistaken for evidence that a plugin can call it.
    expect(content).toContain("PermissionCreateInput");
    expect(content).toContain("issues/37164");
    // Which package LINE the upstream issue is filed against is the load-bearing
    // fact — a V1 defect cannot excuse a V2 row. The version is deliberately not
    // asserted here: it belongs to the upstream issue rather than to our pin,
    // and a literal in a file the harness loads is what the pin guard forbids.
    expect(content).toContain("opencode-ai@");
  });
});

describe("OpenCode V2 acceptance matrix", () => {
  test("every slice S1 through S10 is represented", () => {
    const requiredSlices = ["S1", "S2", "S3", "S4", "S5", "S6", "S7", "S8", "S9", "S10"];
    const presentSlices = new Set(acceptanceMatrix.map((row) => row.sliceId));
    for (const slice of requiredSlices) {
      expect(presentSlices.has(slice as AcceptanceRow["sliceId"])).toBe(true);
    }
  });

  test("every row is keyed to governing source and points to an existing test", () => {
    for (const row of acceptanceMatrix) {
      expect(row.governingSource.length).toBeGreaterThan(0);
      const fullPath = join(pluginTestRoot, row.testFile);
      expect(existsSync(fullPath)).toBe(true);
      const source = readFileSync(fullPath, "utf8");
      expect(source).toContain(row.testPattern);
    }
  });

  test("no row uses self-asserted verified status", () => {
    for (const row of acceptanceMatrix) {
      expect((row as Record<string, unknown>).status).toBeUndefined();
    }
  });
});
