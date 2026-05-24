#!/usr/bin/env node
/**
 * aimock-based OpenAI mock server for AFT Windows E2E tests.
 *
 * Mirrors `tests/docker/mock-server.js` but with scenarios biased toward
 * Windows-specific behaviors:
 *
 *   - bash with various durations (issue #26 reproduction)
 *   - Read/grep/edit through hoisted tools (Windows path normalization)
 *
 * Resolution rule: NODE_PATH must point at the global npm root so the
 * `@copilotkit/aimock` package resolves without a local node_modules.
 * `tests/windows-e2e/run.ps1` sets that env var before spawning us.
 *
 * MATCHING STRATEGY (lessons learned across multiple iterations 2026-05-04):
 *
 * aimock's matchers (router.js:matchFixture) check the LAST user message
 * in the conversation. Each follow-up assistant turn re-uses the SAME
 * original user prompt — so a plain `onMessage(/pattern/, response)`
 * matches AGAIN on every follow-up turn, producing an infinite loop:
 * tool → result → same prompt matches → same tool → result → ...
 *
 * The fix is to combine `userMessage` (which scenario?) with `turnIndex`
 * (which position in the conversation?). aimock exposes this as
 * `onTurn(N, /pattern/, response)`:
 *   - userMessage pattern routes by SCENARIO (S1's prompt vs S2's prompt)
 *   - turnIndex routes by POSITION (0=initial tool call, 1=follow-up,...)
 *
 * `turnIndex` counts the number of assistant messages already in the
 * conversation. So:
 *   turnIndex 0 = first response (no assistant turns yet)
 *   turnIndex 1 = second response (after one tool roundtrip)
 *   ...
 *
 * `turnIndex` matches deterministically once and only when its position
 * is reached, breaking the loop.
 */
const { LLMock } = require("@copilotkit/aimock");
const fs = require("node:fs");
const path = require("node:path");

function parsePort(value) {
    const port = Number.parseInt(value || "0", 10);
    if (!Number.isInteger(port) || port < 0 || port > 65535) {
        throw new Error(`invalid AIMOCK_PORT: ${value}`);
    }
    return port;
}

const port = parsePort(process.env.AIMOCK_PORT);
const journalFile = path.join(
    process.env.TEMP || process.env.TMPDIR || "/tmp",
    "aimock-journal.txt"
);

async function main() {
    const mock = new LLMock({ port });

    // -------------------------------------------------------------------
    // Scenario 1 — basic tools (Outline / Read / Grep / Edit / Undo)
    //
    // The harness prompt for S1 contains "Outline src". Each onTurn entry
    // requires both:
    //   * the user prompt to match /Outline src/
    //   * the assistant turn count to equal the specified N
    //
    // After turn 5 returns the wrap-up text, opencode's session ends
    // naturally — no further completions, no infinite loop.
    // -------------------------------------------------------------------

    mock.onTurn(0, /Outline src/, {
        toolCalls: [
            { name: "aft_outline", arguments: JSON.stringify({ target: "src" }) },
        ],
    });
    mock.onTurn(1, /Outline src/, {
        toolCalls: [
            { name: "read", arguments: JSON.stringify({ filePath: "src/main.py" }) },
        ],
    });
    mock.onTurn(2, /Outline src/, {
        toolCalls: [
            { name: "grep", arguments: JSON.stringify({ pattern: "def ", path: "src" }) },
        ],
    });
    mock.onTurn(3, /Outline src/, {
        toolCalls: [
            {
                name: "edit",
                arguments: JSON.stringify({
                    filePath: "src/main.py",
                    oldString: 'name = "World"',
                    newString: 'name = "Windows"',
                }),
            },
        ],
    });
    mock.onTurn(4, /Outline src/, {
        toolCalls: [
            {
                name: "aft_safety",
                arguments: JSON.stringify({ op: "undo", filePath: "src/main.py" }),
            },
        ],
    });
    mock.onTurn(5, /Outline src/, {
        content: "Outline + read + grep + edit + undo all worked. Done.",
    });

    // -------------------------------------------------------------------
    // Scenario 2 — bash timeout reproduction (issue #26)
    //
    // The S2 prompt contains "bash-timing-test.cmd". Turn 0 issues the
    // bash tool call; turn 1 wraps up after the tool result.
    //
    // The script (created by run.ps1) writes a START timestamp, sleeps
    // 60s via Windows-native `timeout /t 60`, writes an END timestamp.
    // The harness asserts both START + END exist and elapsed ≈ 60s.
    //
    // 65s requested bash timeout puts the bridge transport budget at
    // max(30s, 65s+5s) = 70s — exactly the issue #26 boundary.
    // -------------------------------------------------------------------

    // Important: AFT bash on Windows runs via PowerShell, NOT cmd.exe (the
    // earlier mental model was wrong). PowerShell does NOT search the
    // current directory for executables by default (security feature), so
    // `bash-timing-test.cmd` resolves to nothing and exits 1 without
    // running. We bypass that by issuing a self-contained PowerShell
    // pipeline that:
    //   1. writes the START timestamp
    //   2. sleeps 60 seconds via `Start-Sleep`
    //   3. writes the END timestamp
    //
    // This is identical in semantic to bash-timing-test.cmd from the
    // run.ps1 setup, but executable directly without depending on the
    // PowerShell PATH/cwd-search rules. The .cmd file is no longer used
    // (we keep it created so a future cmd.exe-based AFT shell can use
    // it without changing the harness).
    //
    // The single-line semicolon-separated form keeps everything in ONE
    // PowerShell process, so the START / sleep / END are atomic from
    // the harness's perspective — no race between the .cmd interpreter
    // and the shell.
    mock.onTurn(0, /bash-timing-test/, {
        toolCalls: [
            {
                name: "bash",
                arguments: JSON.stringify({
                    command:
                        '$marker = Join-Path $env:TEMP "bash-timing-marker.txt"; ' +
                        '[DateTime]::UtcNow.ToString("o") | Set-Content $marker; ' +
                        '"START" | Add-Content $marker; ' +
                        "Start-Sleep -Seconds 60; " +
                        '"END" | Add-Content $marker; ' +
                        '[DateTime]::UtcNow.ToString("o") | Add-Content $marker; ' +
                        'Write-Output "bash-timing-test done"',
                    // 75s timeout (was 65s — too tight): PowerShell 5.1
                    // cold-start under detached spawn can take 5-7s before
                    // Start-Sleep begins. With a 60s sleep, that pushes the
                    // child past the 65s boundary intermittently and the
                    // bash tool kills it before the END marker writes,
                    // making the START+END assertion flaky. 75s gives
                    // realistic headroom while still testing both the
                    // timeout-enforcement code path and the v0.19.2 transport
                    // budget fix (max(30s, requested+5s) = 80s, well above
                    // the 75s bash timeout).
                    timeout: 75000,
                    description:
                        "issue #26 boundary: 60s Start-Sleep with 75s timeout",
                }),
            },
        ],
    });
    mock.onTurn(1, /bash-timing-test/, {
        content: "Bash timing test complete.",
    });

    // -------------------------------------------------------------------
    // Scenario 2b — interactive-prompt deadlock (issue #26 root cause)
    //
    // Issue #26 was a 65s bridge transport timeout on Windows bash. The
    // 60s Start-Sleep test above doesn't reproduce it because Start-Sleep
    // doesn't read stdin. The actual root cause was that AFT's foreground
    // bash was inheriting stdin from the bridge process. When a child
    // process (PowerShell prompts, git/npm credential prompts, etc.)
    // tries to read from stdin, it would block forever waiting for input
    // that never arrives — the bridge timed out at 65s.
    //
    // This scenario explicitly invokes Read-Host. With a buggy bash
    // (no stdin=null, no -NonInteractive) on Windows, Read-Host blocks
    // forever waiting for the user to type something. With the fix
    // (stdin=null + -NonInteractive), PowerShell errors out fast with
    // "Read-Host: Cannot prompt user (NonInteractive mode)" — bash
    // returns within seconds, no bridge timeout.
    //
    // Marker file proves whether the bash returned at all. If the marker
    // is written, the fix worked. If the marker is missing AND the plugin
    // log shows a bridge timeout, the bug reproduces.
    //
    // Requested timeout = 10s (we expect failure-to-prompt to return in
    // under a second; 10s is generous and stays well below the hang
    // threshold a user would hit). Bridge transport budget = 30s, so a
    // bug-induced hang would surface as a bridge timeout at ~30s, not
    // a bash-itself timeout.
    // -------------------------------------------------------------------

    mock.onTurn(0, /interactive-prompt-test/, {
        toolCalls: [
            {
                name: "bash",
                arguments: JSON.stringify({
                    command:
                        '$marker = Join-Path $env:TEMP "interactive-marker.txt"; ' +
                        '"BEFORE-PROMPT" | Set-Content $marker; ' +
                        // Read-Host is interactive; with -NonInteractive it errors,
                        // without it (the bug) it blocks forever on stdin.
                        // We swallow the error so the script returns gracefully
                        // either way — what we want to measure is whether bash
                        // RETURNS at all, not its exit code.
                        "try { $answer = Read-Host 'Type something' } catch { $answer = '<no-prompt>' }; " +
                        '"AFTER-PROMPT" | Add-Content $marker; ' +
                        '"answer=$answer" | Add-Content $marker; ' +
                        'Write-Output "interactive-prompt-test done"',
                    timeout: 10000, // 10s — well under any reasonable hang threshold
                    description:
                        "issue #26 root cause: Read-Host on stdin-detached PowerShell",
                }),
            },
        ],
    });
    mock.onTurn(1, /interactive-prompt-test/, {
        content: "Interactive prompt test complete.",
    });

    // -------------------------------------------------------------------
    // Catch-all fallback
    //
    // Should ONLY fire if the conversation continues past the scripted
    // turns above (e.g., model did extra work). Tagged so the harness
    // can detect it as a failure mode.
    // -------------------------------------------------------------------
    mock.onMessage(/.*/, { content: "[AIMOCK_FALLBACK] Task complete." });

    await mock.start();
    console.log(`[aimock] listening on ${mock.url}`);
    console.log(
        `[aimock] scripted: S1 turns 0-5 (Outline src), S2 turns 0-1 (bash-timing-test.cmd)`
    );

    // Periodic journal dump so the harness can see per-request activity
    // from outside aimock's process. Without this, aimock is a black box.
    // We write the count of received chat-completion requests every second.
    const journalInterval = setInterval(() => {
        try {
            const reqs = mock.getRequests();
            const summary = reqs
                .map((r, i) => {
                    const p = r.path || r.url || "?";
                    const ts = r.timestamp || r.time || "?";
                    return `${i}: ${ts} ${p}`;
                })
                .join("\n");
            fs.writeFileSync(
                journalFile,
                `aimock journal — ${reqs.length} requests\n${summary}\n`
            );
        } catch (_e) {
            // Best-effort — don't crash if getRequests changes.
        }
    }, 1000);

    process.on("SIGTERM", async () => {
        clearInterval(journalInterval);
        await mock.stop();
        process.exit(0);
    });
    process.on("SIGINT", async () => {
        clearInterval(journalInterval);
        await mock.stop();
        process.exit(0);
    });
}

main().catch((e) => {
    console.error("[aimock] fatal:", e);
    process.exit(1);
});
