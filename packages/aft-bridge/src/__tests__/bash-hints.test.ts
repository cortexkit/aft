import { describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import {
  commandInvokesCodeSearch,
  maybeAppendConflictsHint,
  maybeAppendGrepSearchHint,
} from "../bash-hints.js";

const AFT_SEARCH_HINT =
  "DO NOT search code by running grep/rg in bash — it is unindexed, unranked, and serial. Use the `aft_search` tool instead (it auto-routes concepts, identifiers, regex, and literals).";
const GREP_TOOL_HINT =
  "DO NOT search code by running grep/rg in bash — it is unindexed, unranked, and serial. Use the `grep` tool instead (indexed and ranked).";

describe("maybeAppendConflictsHint", () => {
  test("appends hint on real git-merge conflict output", () => {
    const output = [
      "Auto-merging packages/opencode-plugin/src/index.ts",
      "CONFLICT (content): Merge conflict in packages/opencode-plugin/src/index.ts",
      "Automatic merge failed; fix conflicts and then commit the result.",
    ].join("\n");
    expect(maybeAppendConflictsHint(output)).toContain("[Hint] Use aft_conflicts");
  });

  test("appends hint on rebase conflict output", () => {
    const output = [
      "error: could not apply 0e3f4a2... feat: add foo",
      "Automatic merge failed; fix conflicts and then commit the result.",
    ].join("\n");
    expect(maybeAppendConflictsHint(output)).toContain("[Hint] Use aft_conflicts");
  });

  // The trigger string appears verbatim in many docs/READMEs. The hint must NOT
  // fire just because someone cat'd a README in a non-git directory.
  test("does NOT append hint when marker appears alone (e.g. README excerpt)", () => {
    const output =
      "When git can't merge automatically, you'll see:\n\n" +
      "  Automatic merge failed; fix conflicts and then commit the result.\n\n" +
      "This means you need to resolve the conflict manually.";
    expect(maybeAppendConflictsHint(output)).toBe(output);
  });

  test("does NOT fire on mid-line CONFLICT substring", () => {
    const output = "we documented: Automatic merge failed; fix conflicts. (see CONFLICT (3) below)";
    expect(maybeAppendConflictsHint(output)).toBe(output);
  });

  test("does NOT append hint when output is unrelated to git", () => {
    const output = "hello world\n+0/-0\nlinting passed.";
    expect(maybeAppendConflictsHint(output)).toBe(output);
  });

  test("does NOT append hint when output is empty", () => {
    expect(maybeAppendConflictsHint("")).toBe("");
  });
});

describe("commandInvokesCodeSearch", () => {
  const positives = [
    'grep -nE "x" src/',
    "grep foo file.ts | head",
    "rg -n pat",
    "cd packages/x && grep -rn foo .",
    "cd \"my dir\" && rg 'p' .",
    '"grep" -n pat file',
    "grep pat file || true",
    'grep "a|b" file | head',
    // grep leading a non-first statement must still nudge (the reported bug):
    "cd x; grep foo",
    "false || grep pat",
    "cd ~/proj && echo '=== marker ===' && grep -rn foo src/ | head -20",
    "cd ~/proj\necho '=== marker ==='\ngrep -rn foo src/ | head -20",
  ];

  const negatives = [
    "bun test | grep fail",
    "cargo build 2>&1 | rg error",
    "echo hi | grep h",
    "make test | grep -i pass",
    "ls -la",
    "FOO=1 grep pat file",
    "2>&1 grep pat",
    'cd "unclosed && grep foo',
    // grep only as a downstream filter across statements must not nudge:
    "cd x && bun test | grep fail",
    "echo 'grep is mentioned here' && ls",
  ];

  for (const command of positives) {
    test(`positive: ${command}`, () => {
      expect(commandInvokesCodeSearch(command)).toBe(true);
    });
  }

  for (const command of negatives) {
    test(`negative: ${command}`, () => {
      expect(commandInvokesCodeSearch(command)).toBe(false);
    });
  }
});

describe("maybeAppendGrepSearchHint", () => {
  const projectRoot = "/some/proj";

  test("appends aft_search hint for a leading grep when aft_search is registered", () => {
    const result = maybeAppendGrepSearchHint("matches", "grep foo file.ts", true);
    expect(result).toBe(`matches\n\n${AFT_SEARCH_HINT}`);
  });

  test("appends grep-tool hint for a leading grep when aft_search is not registered", () => {
    const result = maybeAppendGrepSearchHint("matches", "grep foo file.ts", false);
    expect(result).toBe(`matches\n\n${GREP_TOOL_HINT}`);
  });

  test("does NOT append for a piped filtering grep", () => {
    const output = "failure details";
    expect(maybeAppendGrepSearchHint(output, "bun test | grep fail", true)).toBe(output);
    expect(maybeAppendGrepSearchHint(output, "bun test | grep fail", false)).toBe(output);
  });

  test("does NOT append when output is empty", () => {
    expect(maybeAppendGrepSearchHint("", "grep foo file.ts", true)).toBe("");
  });

  test("does NOT double-append an existing grep search hint", () => {
    const output = `matches\n\n${AFT_SEARCH_HINT}`;
    expect(maybeAppendGrepSearchHint(output, "grep foo file.ts", true)).toBe(output);
  });

  test("does NOT append when grep targets only paths outside projectRoot", () => {
    const output = "config line";
    expect(
      maybeAppendGrepSearchHint(
        output,
        "grep -A6 '\"semantic\"' ~/.pi/agent/aft.jsonc",
        true,
        projectRoot,
      ),
    ).toBe(output);
    expect(
      maybeAppendGrepSearchHint(output, "grep x ~/.config/opencode/aft.jsonc", true, projectRoot),
    ).toBe(output);
    expect(maybeAppendGrepSearchHint(output, "grep foo /etc/hosts", true, projectRoot)).toBe(
      output,
    );
  });

  test("appends when grep has no explicit path operand (searches project cwd)", () => {
    const result = maybeAppendGrepSearchHint("hits", "grep -rn foo", true, projectRoot);
    expect(result).toBe(`hits\n\n${AFT_SEARCH_HINT}`);
  });

  // A `$VAR` target carries no slash, so the operand collector used to see a
  // path-less grep and nudge toward aft_search for a target it could not
  // resolve (a log file under the storage dir, in the field). An unknowable
  // target must suppress; a dynamic PATTERN with a real in-project path must
  // not (the control keeps the nudge honest).
  test("does NOT append when the grep target is a shell variable", () => {
    const output = "log lines";
    expect(
      maybeAppendGrepSearchHint(
        output,
        'L=~/.local/share/x/logs/a.log; grep -n "abc123" $L | tail -8 | cut -c1-230',
        true,
        projectRoot,
      ),
    ).toBe(output);
    expect(maybeAppendGrepSearchHint(output, "grep -rn foo $DIR", true, projectRoot)).toBe(output);
    expect(maybeAppendGrepSearchHint("hits", "grep $PAT ./src/file.ts", true, projectRoot)).toBe(
      `hits\n\n${AFT_SEARCH_HINT}`,
    );
  });

  test("appends when grep includes an in-project relative path", () => {
    const result = maybeAppendGrepSearchHint("hits", "grep foo ./src/file.ts", true, projectRoot);
    expect(result).toBe(`hits\n\n${AFT_SEARCH_HINT}`);
  });

  test("does NOT append when grep is buried after other statements and paths are outside", () => {
    const output = "ok";
    expect(
      maybeAppendGrepSearchHint(output, "cd x && echo y && grep z ~/outside/f", true, projectRoot),
    ).toBe(output);
  });

  test("does NOT append when a cd into another repo precedes a relative-path grep (#issue)", () => {
    // The bug: `cd <other-repo> && grep foo tools/bash.ts` resolved the relative
    // operand against the SESSION root, looked in-project, and fired — even though
    // the grep runs in a different repo aft_search can't search.
    const output = "match in another repo";
    expect(
      maybeAppendGrepSearchHint(
        output,
        "cd /other/repo/src && grep -n foo tools/bash.ts",
        true,
        projectRoot,
      ),
    ).toBe(output);
    // Multi-line cd;…;grep form (as a host may send it).
    expect(
      maybeAppendGrepSearchHint(
        output,
        "cd /other/repo\necho scanning\ngrep -n foo tools/bash.ts",
        true,
        projectRoot,
      ),
    ).toBe(output);
  });

  test("still appends when a cd stays inside the project, then greps a relative path", () => {
    const result = maybeAppendGrepSearchHint(
      "hits",
      "cd src && grep -n foo file.ts",
      true,
      projectRoot,
    );
    expect(result).toBe(`hits\n\n${AFT_SEARCH_HINT}`);
  });

  test("does NOT append when cd target is dynamic (cwd unknown → cannot confirm in-project)", () => {
    const output = "match";
    expect(
      maybeAppendGrepSearchHint(output, "cd $DIR && grep -n foo file.ts", true, projectRoot),
    ).toBe(output);
  });

  test("does NOT append when grep path operand is dynamic", () => {
    const output = "match";
    expect(maybeAppendGrepSearchHint(output, 'grep x "$HOME/foo"', true, projectRoot)).toBe(output);
    expect(maybeAppendGrepSearchHint(output, 'grep x "$HOME/foo"', true)).toBe(output);
    expect(maybeAppendGrepSearchHint(output, "grep x '$PROJECT/foo'", true, projectRoot)).toBe(
      output,
    );
  });

  test("appends when mixed operands include an in-project path", () => {
    const result = maybeAppendGrepSearchHint(
      "hits",
      "grep -f ~/pat.txt foo src/",
      true,
      projectRoot,
    );
    expect(result).toBe(`hits\n\n${AFT_SEARCH_HINT}`);
  });

  test("preserves always-nudge behavior when projectRoot is empty or undefined", () => {
    const output = "hits";
    expect(maybeAppendGrepSearchHint(output, "grep x src/file.ts", true)).toBe(
      `${output}\n\n${AFT_SEARCH_HINT}`,
    );
    expect(maybeAppendGrepSearchHint(output, "grep x src/file.ts", true, "")).toBe(
      `${output}\n\n${AFT_SEARCH_HINT}`,
    );
    expect(maybeAppendGrepSearchHint(output, "grep x src/file.ts", true, "   ")).toBe(
      `${output}\n\n${AFT_SEARCH_HINT}`,
    );
  });

  test("suppresses a piped grep targeting an in-project file", () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "aft-bash-hints-"));
    try {
      fs.mkdirSync(path.join(root, "src"));
      const file = path.join(root, "src/app.ts");
      fs.writeFileSync(file, "foo\n");
      const old = new Date(Date.now() - 61_000);
      fs.utimesSync(file, old, old);
      expect(maybeAppendGrepSearchHint("hit", "grep -n foo src/app.ts | head -5", true, root)).toBe(
        "hit",
      );
    } finally {
      fs.rmSync(root, { recursive: true, force: true });
    }
  });

  test("keeps the lecture for a piped old in-project directory search", () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "aft-bash-hints-"));
    try {
      const src = path.join(root, "src");
      fs.mkdirSync(src);
      fs.writeFileSync(path.join(src, "app.ts"), "foo\n");
      const old = new Date(Date.now() - 61_000);
      fs.utimesSync(src, old, old);
      expect(maybeAppendGrepSearchHint("hit", "grep -n foo src | head -5", true, root)).toContain(
        AFT_SEARCH_HINT,
      );
    } finally {
      fs.rmSync(root, { recursive: true, force: true });
    }
  });

  test("suppresses a piped grep targeting a fresh in-project file", () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "aft-bash-hints-"));
    try {
      fs.mkdirSync(path.join(root, "src"));
      fs.writeFileSync(path.join(root, "src/app.ts"), "foo\n");
      expect(maybeAppendGrepSearchHint("hit", "grep -n foo src/app.ts | head -5", true, root)).toBe(
        "hit",
      );
    } finally {
      fs.rmSync(root, { recursive: true, force: true });
    }
  });

  test("keeps the lecture for a piped grep targeting a nonexistent path", () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "aft-bash-hints-"));
    try {
      expect(
        maybeAppendGrepSearchHint("hit", "grep -n foo src/missing.ts | head -5", true, root),
      ).toContain(AFT_SEARCH_HINT);
    } finally {
      fs.rmSync(root, { recursive: true, force: true });
    }
  });

  test("suppresses a multi-operand grep when any operand is an existing file", () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "aft-bash-hints-"));
    try {
      const src = path.join(root, "src");
      fs.mkdirSync(src);
      fs.writeFileSync(path.join(src, "app.ts"), "foo\n");
      const old = new Date(Date.now() - 61_000);
      fs.utimesSync(src, old, old);
      expect(
        maybeAppendGrepSearchHint("hit", "grep -n foo src/app.ts src/ | head -5", true, root),
      ).toBe("hit");
    } finally {
      fs.rmSync(root, { recursive: true, force: true });
    }
  });
});

describe("maybeAppendGrepSearchHint — redirection operand scan terminates", () => {
  // Regression: collectPathOperands looped forever when a grep statement
  // contained a redirection (`2>/dev/null`). readShellToken parks on `>` and
  // returns an empty token without advancing, so the operand `while` spun
  // without progress and blocked the event loop (hung the host). A reaching
  // value (not a timeout) here proves the scan now terminates.
  const PROJECT_ROOT = "/Users/dev/proj";

  test("the reported hang command returns instead of looping", () => {
    const command = [
      "cd ~/proj/packages/plugin",
      'echo "=== does it PREPEND or APPEND? synthetic? ==="',
      'grep -rnE "synthetic|unshift|push\\(|role:\\s*\\"user\\"|parts\\.push|\\.text \\+=|ctx-search-hint|messages\\.splice" src/hooks/auto-search-hint.ts 2>/dev/null | head -25',
      'echo ""',
      "ls src/hooks/auto-search*.ts 2>/dev/null",
    ].join("\n");
    // Must return (terminate); value itself is not the point.
    const out = maybeAppendGrepSearchHint("matches found", command, true, PROJECT_ROOT);
    expect(typeof out).toBe("string");
  });

  test("grep with redirection still collects the in-project path operand (hint fires)", () => {
    const command = "grep -rn foo src/index.ts 2>/dev/null";
    const out = maybeAppendGrepSearchHint("hit", command, true, PROJECT_ROOT);
    expect(out).toContain(AFT_SEARCH_HINT);
  });

  test("grep with redirection to an out-of-project path is suppressed", () => {
    const command = "grep -rn foo /etc/hosts 2>/dev/null";
    const out = maybeAppendGrepSearchHint("hit", command, true, PROJECT_ROOT);
    expect(out).toBe("hit");
  });

  test("grep redirection with no path operand (recursive cwd) still fires", () => {
    const command = "grep -rn foo 2>/dev/null";
    const out = maybeAppendGrepSearchHint("hit", command, true, PROJECT_ROOT);
    expect(out).toContain(AFT_SEARCH_HINT);
  });
});
