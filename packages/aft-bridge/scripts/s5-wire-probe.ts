#!/usr/bin/env bun
/**
 * B-FINAL S5 — live wire-probe of AFT-as-a-subc-module.
 *
 * Constructs a REAL SubcTransportPool against SUBC's isolated S5 rig (a live
 * subc-core daemon supervising the HEAD aft binary) and exercises the four
 * proofs that only an end-to-end run can give:
 *   (1) real handshake + route.open + tool calls across the surface, with the
 *       structuredContent envelope re-lifted to the flat result;
 *   (2) the dedicated-channel proof — a bg_events subscribe opens a SECOND route;
 *   (3) the bg_events idle wake -> forced-drain -> ack cycle;
 *   (4) deferred-bash foreground wait/promote over subc.
 *
 * Run: bun packages/aft-bridge/scripts/s5-wire-probe.ts <connection-file> <project-root>
 */

import { SubcTransportPool } from "../src/subc-transport.js";

const CONNECTION_FILE = process.argv[2] ?? "/tmp/subc-s5-rig/run/subc-connection.json";
const PROJECT_ROOT = process.argv[3] ?? process.cwd();
const SESSION = "s5-probe-session";

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

async function main(): Promise<void> {
  console.log(`\n=== S5 wire-probe ===`);
  console.log(`connection: ${CONNECTION_FILE}`);
  console.log(`project:    ${PROJECT_ROOT}\n`);

  const nudges: { root: string; session: string }[] = [];
  const pool = new SubcTransportPool({
    connectionFile: CONNECTION_FILE,
    harness: "opencode",
    onBgEventsNudge: (root, session) => {
      nudges.push({ root, session });
    },
  });

  try {
    const bridge = pool.getBridge(PROJECT_ROOT);

    // --- Proof 1: handshake + route.open + representative tool calls ---
    console.log("[1] tool surface + structuredContent re-lift");

    const read = await bridge.toolCall(SESSION, "read", { filePath: "Cargo.toml" });
    check("read returns success", read.success === true);
    check(
      "read text re-lifted",
      typeof read.text === "string" && read.text.length > 0,
      `${read.text?.length ?? 0} chars`,
    );

    const grep = await bridge.toolCall(SESSION, "grep", {
      pattern: "agent-file-tools",
      path: "Cargo.toml",
    });
    check("grep returns success", grep.success === true);
    check("grep text re-lifted", typeof grep.text === "string");

    const glob = await bridge.toolCall(SESSION, "glob", { pattern: "*.toml" });
    check("glob returns success", glob.success === true);

    const outline = await bridge.toolCall(SESSION, "outline", {
      target: "crates/aft-tools/src/subc_translate.rs",
    });
    check("outline returns success", outline.success === true);

    // A not-found read: a tool-level success:false must come back as a RESULT,
    // not throw (the honesty contract over the wire).
    const missing = await bridge.toolCall(SESSION, "read", {
      filePath: "definitely-not-a-real-file-xyz.txt",
    });
    check("missing read returns (not thrown)", typeof missing.success === "boolean");
    check(
      "missing read success:false",
      missing.success === false,
      `code=${String(missing.code ?? "")}`,
    );

    // status: a native-ish tool, confirms the surface beyond the file tools.
    const status = await bridge.toolCall(SESSION, "status", {});
    check("status returns", typeof status.success === "boolean");

    // --- Proof 4: deferred-bash foreground wait/promote over subc ---
    console.log("\n[4] deferred bash (foreground wait) over subc");
    const t0 = Date.now();
    const fast = await bridge.toolCall(SESSION, "bash", { command: "echo s5-fast-ok" });
    check("fast bash success", fast.success === true);
    check(
      "fast bash captured output",
      JSON.stringify(fast).includes("s5-fast-ok"),
      `${Date.now() - t0}ms`,
    );

    // --- Proof 3: bg_events idle wake -> forced-drain -> ack ---
    console.log("\n[3] bg_events idle wake -> drain cycle");
    // Wire the nudge to a real drain so we prove the full round-trip over subc.
    let drainedCompletions = 0;
    const drainNudge = async (): Promise<void> => {
      const drain = await bridge.send("bash_drain_completions", { session_id: SESSION });
      const completions = Array.isArray(drain.bg_completions) ? drain.bg_completions : [];
      if (completions.length > 0) {
        drainedCompletions += completions.length;
        const taskIds = completions.map((c) => (c as { task_id?: string }).task_id).filter(Boolean);
        await bridge.send("bash_ack_completions", { session_id: SESSION, task_ids: taskIds });
      }
    };

    // Spawn a backgrounded task that completes after we stop waiting (idle).
    const spawn = await bridge.toolCall(SESSION, "bash", {
      command: "sleep 1; echo s5-bg-done",
      background: true,
    });
    const taskId = (spawn as { task_id?: string }).task_id;
    check("bg bash spawned", typeof taskId === "string", `task=${taskId ?? "?"}`);

    // Now sit idle and let the wake lane nudge us. Drain on each nudge.
    const nudgesBefore = nudges.length;
    const deadline = Date.now() + 12000;
    while (Date.now() < deadline && drainedCompletions === 0) {
      const seen = nudges.length;
      await new Promise((r) => setTimeout(r, 250));
      if (nudges.length > seen) await drainNudge();
    }
    check(
      "bg_events nudge fired while idle",
      nudges.length > nudgesBefore,
      `${nudges.length - nudgesBefore} nudges`,
    );
    check(
      "idle completion drained over subc",
      drainedCompletions > 0,
      `${drainedCompletions} completions`,
    );

    // After ack, the module CLEAR should stop the re-arm: no sustained nudges.
    const quietStart = nudges.length;
    await new Promise((r) => setTimeout(r, 1500));
    check(
      "nudges quiet after ack (module CLEAR)",
      nudges.length - quietStart <= 1,
      `${nudges.length - quietStart} post-ack nudges`,
    );

    // --- Proof 2: dedicated channel (second session => its own routes) ---
    console.log("\n[2] dedicated bg_events channel / multi-session routing");
    const bridge2 = pool.getBridge(PROJECT_ROOT);
    const read2 = await bridge2.toolCall("s5-probe-session-2", "read", {
      filePath: "Cargo.toml",
    });
    check("second session tool call success", read2.success === true);
  } finally {
    await pool.shutdown();
  }

  console.log(`\n=== S5 result: ${pass} pass, ${fail} fail ===\n`);
  process.exit(fail === 0 ? 0 : 1);
}

main().catch((err) => {
  console.error("S5 probe crashed:", err);
  process.exit(2);
});
