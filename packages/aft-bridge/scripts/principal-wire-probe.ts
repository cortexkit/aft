#!/usr/bin/env bun
/**
 * Principal wire-probe — live verification of AFT's bind-trust enforcement
 * against a REAL stamping daemon (universal spawn attestation).
 *
 * Drives the four-case matrix through a genuine subc daemon:
 *   (1) DIRECT bind (no consumer_identity) → trusted: out-of-root write honored
 *       (restrict_to_project_root=false default), bash spawns. Uses harness
 *       "mcp:probe" to prove harness is COSMETIC (mcp harness + direct principal
 *       must still be trusted).
 *   (2) FACADE bind (consumer_identity{module_id:"subc-mcp", launch_nonce:<real>})
 *       → stamped reserved:subc-mcp → untrusted: bash → bash_denied_untrusted,
 *       out-of-root write → rejected, in-root ops work. Uses harness "runner"
 *       (anti-spoof: runner harness + facade principal must still be DENIED).
 *   (3) Both binds concurrently on ONE root: trusted unaffected by the
 *       untrusted sibling.
 *   (4) BAD NONCE route.open → daemon rejects with bad_consumer_identity,
 *       nothing is relayed to the module.
 *
 * Prereqs: a rig daemon with a spawn-attested "subc-mcp" pseudo-module whose
 * nonce is dumped to <rig>/runtime/subc-mcp.nonce, and the HEAD aft binary
 * self-connected as provider: `target/debug/aft --subc <connection-file>`.
 *
 * Run: bun packages/aft-bridge/scripts/principal-wire-probe.ts \
 *        <connection-file> <nonce-file> <project-root>
 */

import { readFileSync } from "node:fs";
// Requires @cortexkit/subc-client >=0.2.0: earlier versions predate
// consumer_identity and silently drop the option (the bind then goes out
// identity-less and gets stamped `direct`).
import { SubcClient } from "@cortexkit/subc-client";

const CONNECTION_FILE = process.argv[2] ?? "/tmp/subc-principal-rig/runtime/subc-connection.json";
const NONCE_FILE = process.argv[3] ?? "/tmp/subc-principal-rig/runtime/subc-mcp.nonce";
const PROJECT_ROOT = process.argv[4] ?? "/tmp/subc-principal-rig/probe-project";
const OUTSIDE_FILE = "/tmp/subc-principal-probe-outside.txt";

let pass = 0;
let fail = 0;
function check(name: string, ok: boolean, detail?: string): void {
  if (ok) {
    pass += 1;
    console.log(`  PASS  ${name}${detail ? ` — ${detail}` : ""}`);
  } else {
    fail += 1;
    console.log(`  FAIL  ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

type FlatResult = { success?: boolean; code?: string; text?: string } & Record<string, unknown>;

function relift(reply: unknown): FlatResult {
  const rec = reply as Record<string, unknown> | null;
  const sc = rec?.structuredContent as FlatResult | undefined;
  return sc ?? (rec as FlatResult) ?? {};
}

async function main(): Promise<void> {
  console.log("\n=== principal wire-probe (real spawn attestation) ===");
  console.log(`connection: ${CONNECTION_FILE}`);
  console.log(`nonce file: ${NONCE_FILE}`);
  console.log(`project:    ${PROJECT_ROOT}\n`);

  const nonce = readFileSync(NONCE_FILE, "utf-8").trim();
  const client = await SubcClient.connect({ connectionFile: CONNECTION_FILE });

  try {
    // --- Case 1: DIRECT bind, harness deliberately "mcp:probe" (anti-spoof) ---
    console.log("[1] direct bind (harness=mcp:probe, no consumer_identity) → trusted");
    const direct = await client.routeOpen(
      { kind: "tool_provider", module_id: "aft" },
      { project_root: PROJECT_ROOT, harness: "mcp:probe", session: "principal-probe" },
      { consumerIdentity: null },
    );

    const dWrite = relift(
      await client.request(direct, {
        name: "write",
        arguments: { filePath: OUTSIDE_FILE, content: "direct-trusted\n" },
      }),
    );
    check(
      "direct: out-of-root write honored",
      dWrite.success === true,
      `code=${dWrite.code ?? ""}`,
    );

    const dBash = relift(
      await client.request(direct, {
        name: "bash",
        arguments: {
          params: { command: "echo principal-direct-ok", description: "Print the trusted marker" },
        },
      }),
    );
    check(
      "direct: bash spawns",
      dBash.success === true && JSON.stringify(dBash).includes("principal-direct-ok"),
    );

    // --- Case 2: FACADE bind, real spawn-attested nonce, harness "runner" (anti-spoof) ---
    console.log("\n[2] facade bind (harness=runner, real subc-mcp nonce) → untrusted");
    const facade = await client.routeOpen(
      { kind: "tool_provider", module_id: "aft" },
      { project_root: PROJECT_ROOT, harness: "runner", session: "principal-probe-facade" },
      { consumerIdentity: { module_id: "subc-mcp", launch_nonce: nonce } },
    );

    const fBash = relift(
      await client.request(facade, {
        name: "bash",
        arguments: {
          params: { command: "echo should-never-run", description: "Run the denied facade probe" },
        },
      }),
    );
    check(
      "facade: bash denied",
      fBash.success === false && fBash.code === "bash_denied_untrusted",
      `code=${fBash.code ?? ""}`,
    );

    const fStatusTool = relift(
      await client.request(facade, { name: "bash_drain_completions", arguments: {} }),
    );
    check(
      "facade: bash_drain_completions denied",
      fStatusTool.success === false && fStatusTool.code === "bash_denied_untrusted",
      `code=${fStatusTool.code ?? ""}`,
    );

    const fOutside = relift(
      await client.request(facade, {
        name: "write",
        arguments: { filePath: OUTSIDE_FILE, content: "facade-should-be-blocked\n" },
      }),
    );
    check(
      "facade: out-of-root write rejected",
      fOutside.success === false,
      `code=${fOutside.code ?? ""}`,
    );

    const fInside = relift(
      await client.request(facade, {
        name: "write",
        arguments: { filePath: `${PROJECT_ROOT}/facade-in-root.txt`, content: "in-root ok\n" },
      }),
    );
    check("facade: in-root write works", fInside.success === true, `code=${fInside.code ?? ""}`);

    const fRead = relift(
      await client.request(facade, {
        name: "read",
        arguments: { filePath: `${PROJECT_ROOT}/facade-in-root.txt` },
      }),
    );
    check("facade: in-root read works", fRead.success === true);

    // --- Case 3: concurrent trusted + untrusted on one root ---
    console.log("\n[3] concurrent binds on one root — trusted unaffected");
    const dWrite2 = relift(
      await client.request(direct, {
        name: "write",
        arguments: { filePath: OUTSIDE_FILE, content: "direct-still-trusted\n" },
      }),
    );
    check("direct bind still writes out-of-root", dWrite2.success === true);
    const dBash2 = relift(
      await client.request(direct, {
        name: "bash",
        arguments: { params: { command: "echo still-ok" } },
      }),
    );
    check("direct bind still runs bash", dBash2.success === true);

    // --- Case 4: bad nonce → daemon-side reject, no RouteBind relayed ---
    console.log("\n[4] bad nonce → route.open rejected by the daemon");
    let rejected: unknown = null;
    try {
      await client.routeOpen(
        { kind: "tool_provider", module_id: "aft" },
        { project_root: PROJECT_ROOT, harness: "mcp:evil", session: "principal-probe-bad" },
        { consumerIdentity: { module_id: "subc-mcp", launch_nonce: "deadbeef" } },
      );
    } catch (err) {
      rejected = err;
    }
    const rejectText =
      rejected instanceof Error
        ? `${(rejected as { code?: string }).code ?? ""} ${rejected.message}`
        : "";
    check(
      "bad nonce rejected (bad_consumer_identity)",
      rejected !== null && rejectText.includes("bad_consumer_identity"),
      rejectText.trim(),
    );
  } finally {
    client.close();
  }

  console.log(`\n=== ${pass} pass, ${fail} fail ===`);
  process.exit(fail > 0 ? 1 : 0);
}

main().catch((err) => {
  console.error("probe crashed:", err);
  process.exit(1);
});
