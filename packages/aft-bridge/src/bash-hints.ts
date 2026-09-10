import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

// Helpers for the bash-output hint nudges appended to bash tool results.
//
// Shared across harnesses (OpenCode applies it in `tool.execute.after`; Pi
// applies it inside its hoisted bash tool). Returns the new output string (or
// the original when no hint should fire). The appended "[Hint] ..." line is
// agent-visible and persists in the tool result.

/**
 * Appended to a bash_watch reply whose sync deadline passed without a match.
 * The deadline is a property of the watch, not of the command: a delegated
 * worker that read the bare "timeout reached" line as its own execution being
 * interrupted declared a failed result while its 12-minute docker matrix was
 * still running. The sentence names the state and the move in both roles.
 */
export const WATCH_TIMEOUT_STEER =
  "The command is still running; this deadline is not a failure. Call bash_watch again or do other work in this turn. Only a primary session may end its turn on it (the completion reminder wakes it); a delegated worker must not end its turn or declare a result while the command runs.";

const CONFLICT_HINT =
  "\n\n[Hint] Use aft_conflicts to see all conflict regions across files in a single call.";

const GREP_SEARCH_AFT_SEARCH_HINT =
  "DO NOT search code by running grep/rg in bash — it is unindexed, unranked, and serial. Use the `aft_search` tool instead (it auto-routes concepts, identifiers, regex, and literals).";

const GREP_SEARCH_GREP_HINT =
  "DO NOT search code by running grep/rg in bash — it is unindexed, unranked, and serial. Use the `grep` tool instead (indexed and ranked).";

const GREP_SEARCH_HINT_PREFIX = "DO NOT search code by running grep/rg in bash —";
const GREP_SEARCH_FRESHNESS_WINDOW_MS = 60_000;

type Quote = "none" | "single" | "double";

interface TokenResult {
  token: string;
  end: number;
}

/**
 * Append the `aft_conflicts` hint when the output indicates a real git merge
 * or rebase produced conflicts.
 *
 * Gated on BOTH:
 *  - the "Automatic merge failed; fix conflicts" marker, AND
 *  - a git-conflict signal (`CONFLICT (...)` line or `error: could not apply`)
 *
 * Both conditions are required because `aft_conflicts` calls `git ls-files -u`,
 * which fails with "not a git repository" outside a git working tree. The
 * marker string can legitimately appear in docs, READMEs, test fixtures, and
 * grep output, so we cannot key off it alone — a false-positive hint sends
 * agents into a confusing error.
 */
export function maybeAppendConflictsHint(output: string): string {
  if (!output.includes("Automatic merge failed; fix conflicts")) return output;
  // git merge prints "CONFLICT (content|file|...): ..." per file.
  // git rebase / git am print "error: could not apply <sha>" per failed pick.
  if (!/^CONFLICT \(|^error: could not apply /m.test(output)) return output;
  return output + CONFLICT_HINT;
}

/**
 * Return true when any top-level statement of the command invokes a code-search
 * command (grep/rg) as the first stage of its pipeline.
 *
 * Splits the command into top-level statements (`&&`, `||`, `;`, `&`, newline)
 * so a search buried after `cd`/`echo` (e.g. `cd x && echo y && grep z`, or a
 * multi-line script) is still detected. grep/rg used as a downstream filter
 * (`bun test | grep fail`) is ignored because it is not the first pipeline
 * stage of its statement. Ambiguous shell syntax (unbalanced quotes/backticks)
 * returns false so the nudge never fires spuriously.
 */
export function commandInvokesCodeSearch(command: string): boolean {
  const statements = splitTopLevelStatements(command);
  if (statements === null) return false;

  for (const statement of statements) {
    const firstStage = firstPipelineStage(statement);
    if (firstStage === null) continue;
    const firstToken = readShellToken(firstStage, skipSpaces(firstStage, 0));
    if (firstToken === null) continue;
    if (firstToken.token === "grep" || firstToken.token === "rg") return true;
  }
  return false;
}

/**
 * Append the grep/rg code-search nudge for native bash output that did not go
 * through the Rust grep rewrite footer path.
 */
export function maybeAppendGrepSearchHint(
  output: string,
  command: string,
  aftSearchRegistered: boolean,
  projectRoot?: string,
): string {
  if (output === "") return output;
  if (!commandInvokesCodeSearch(command)) return output;
  if (output.includes(GREP_SEARCH_HINT_PREFIX)) return output;
  if (shouldSuppressGrepSearchHint(command, projectRoot)) return output;

  const hint = aftSearchRegistered ? GREP_SEARCH_AFT_SEARCH_HINT : GREP_SEARCH_GREP_HINT;
  return `${output}\n\n${hint}`;
}

function shouldSuppressGrepSearchHint(command: string, projectRoot: string | undefined): boolean {
  const statements = splitTopLevelStatements(command);
  if (statements === null) return false;

  if (allSearchPathOperandsAreDynamic(statements)) return true;

  const root = projectRoot?.trim();
  if (!root) return false;

  const resolvedRoot = path.resolve(root);
  // Track the effective cwd across top-level statements so a `cd` into another
  // repo before the grep is honored: aft_search only indexes THIS project, so a
  // grep that runs outside the project root must NOT be nudged toward it. `null`
  // means the cwd became unknown (dynamic/`cd -`) and we can't confirm the grep
  // is in-project — treat such greps as external (suppress).
  let effectiveCwd: string | null = resolvedRoot;
  let sawCodeSearchStatement = false;

  for (const statement of statements) {
    const firstStage = firstPipelineStage(statement);
    if (firstStage === null) continue;
    const firstToken = readShellToken(firstStage, skipSpaces(firstStage, 0));
    if (firstToken === null) continue;
    const head = firstToken.token;

    if (head === "cd") {
      effectiveCwd = nextCwdAfterCd(effectiveCwd, firstStage, firstToken.end);
      continue;
    }

    if (head !== "grep" && head !== "rg") continue;
    sawCodeSearchStatement = true;

    // A grep whose effective cwd is unknown or outside the project is external —
    // don't fire for it. Keep scanning in case a later statement greps in-project.
    if (effectiveCwd === null) continue;
    if (!isDirInsideProject(resolvedRoot, effectiveCwd)) continue;

    const operands = collectPathOperands(firstStage, firstToken.end);
    // No path operands → grep reads stdin or searches the (in-project) cwd.
    if (operands.length === 0) return false;
    let sawInProjectOperand = false;
    for (const operand of operands) {
      if (isDynamicPathOperand(operand)) continue;
      const resolvedOperand = resolvePathOperand(effectiveCwd, operand);
      if (!isPathInsideProject(resolvedRoot, effectiveCwd, operand)) continue;
      sawInProjectOperand = true;
      if (shouldSuppressResolvedPath(resolvedOperand)) return true;
    }
    // All operands resolve outside the project → this grep is external.
    if (sawInProjectOperand) return false;
  }

  return sawCodeSearchStatement;
}

function allSearchPathOperandsAreDynamic(statements: string[]): boolean {
  let sawDynamicOnlySearch = false;
  for (const statement of statements) {
    const firstStage = firstPipelineStage(statement);
    if (firstStage === null) continue;
    const firstToken = readShellToken(firstStage, skipSpaces(firstStage, 0));
    if (firstToken === null) continue;
    const head = firstToken.token;
    if (head !== "grep" && head !== "rg") continue;

    const operands = collectPathOperands(firstStage, firstToken.end);
    if (operands.length === 0) return false;
    if (operands.some((operand) => !isDynamicPathOperand(operand))) return false;
    sawDynamicOnlySearch = true;
  }
  return sawDynamicOnlySearch;
}

/**
 * Resolve the cwd after a top-level `cd` statement. Returns the new absolute
 * cwd, or `null` when it can't be determined (a dynamic target like `$DIR`/
 * `$(...)`, `cd -`, or an already-unknown starting cwd). Flags (`cd -P dir`) are
 * skipped; a bare `cd` (no operand) resolves to the home directory.
 */
function nextCwdAfterCd(
  currentCwd: string | null,
  firstStage: string,
  afterCd: number,
): string | null {
  let index = skipSpaces(firstStage, afterCd);
  let target: string | null = null;
  while (index < firstStage.length) {
    const tokenResult = readShellToken(firstStage, index);
    if (tokenResult === null) break;
    const { token, end } = tokenResult;
    if (end <= index) break; // parked on an operator boundary
    index = skipSpaces(firstStage, end);
    if (token === "-") return null; // `cd -` → previous dir, unknown
    if (token.length > 1 && token.startsWith("-")) continue; // a flag like -P/-L
    target = token;
    break;
  }
  if (target === null) return path.resolve(os.homedir()); // bare `cd` → home
  if (target.includes("$") || target.includes("`")) return null; // dynamic
  const expanded = expandTilde(target);
  if (path.isAbsolute(expanded)) return path.resolve(expanded);
  if (currentCwd === null) return null;
  return path.resolve(currentCwd, expanded);
}

function isDirInsideProject(resolvedRoot: string, dir: string): boolean {
  const rel = path.relative(resolvedRoot, path.resolve(dir));
  return rel === "" || (!rel.startsWith("..") && !path.isAbsolute(rel));
}

function collectPathOperands(firstStage: string, startAfterCommand: number): string[] {
  const operands: string[] = [];
  let index = skipSpaces(firstStage, startAfterCommand);

  while (index < firstStage.length) {
    const tokenResult = readShellToken(firstStage, index);
    if (tokenResult === null) break;
    const { token, end } = tokenResult;
    // No forward progress means readShellToken is parked on a redirection/
    // operator boundary char (`<`, `>`, `|`, `;`, `&`) that it returns as an
    // empty token without advancing. Stop here: grep's search-path operands
    // precede any redirection, and continuing would spin forever (e.g.
    // `grep p f 2>/dev/null` parks on `>`). This guarantees loop termination.
    if (end <= index) break;
    index = skipSpaces(firstStage, end);

    if (token.startsWith("-")) continue;
    if (looksLikePathOperand(token)) operands.push(token);
  }

  return operands;
}

function looksLikePathOperand(token: string): boolean {
  return (
    token.includes("/") ||
    token.startsWith("~") ||
    token.startsWith("./") ||
    token.startsWith("../")
  );
}

function isDynamicPathOperand(token: string): boolean {
  return token.startsWith("~") || token.includes("$") || token.includes("`");
}

function expandTilde(target: string): string {
  if (!target.startsWith("~")) return target;
  if (target === "~" || target.startsWith("~/")) {
    return path.join(os.homedir(), target.slice(1));
  }
  return target;
}

function resolvePathOperand(projectRoot: string, operand: string): string {
  const expanded = expandTilde(operand);
  return path.isAbsolute(expanded) ? path.resolve(expanded) : path.resolve(projectRoot, expanded);
}

function isPathInsideProject(resolvedRoot: string, baseCwd: string, operand: string): boolean {
  // Relative operands resolve against the grep's effective cwd (which may differ
  // from the project root after a `cd`), absolute/`~` operands against the FS.
  const resolved = resolvePathOperand(baseCwd, operand);
  const rel = path.relative(resolvedRoot, resolved);
  return rel === "" || (!rel.startsWith("..") && !path.isAbsolute(rel));
}

function shouldSuppressResolvedPath(resolved: string): boolean {
  try {
    const stats = fs.statSync(resolved);
    if (stats.isFile()) return true;
    const ageMs = Date.now() - stats.mtimeMs;
    return ageMs >= 0 && ageMs < GREP_SEARCH_FRESHNESS_WINDOW_MS;
  } catch {
    return false;
  }
}

/**
 * Split a command into top-level statements, breaking on `&&`, `||`, `;`, `&`,
 * and newlines while respecting quotes, escapes, backticks, and parentheses
 * (separators inside those constructs stay within the statement). A single `|`
 * is a pipe, NOT a statement separator, so it stays inside the statement for
 * the caller to inspect the first pipeline stage. Returns null when quoting is
 * unbalanced so the nudge never fires on ambiguous input.
 */
function splitTopLevelStatements(command: string): string[] | null {
  const statements: string[] = [];
  let start = 0;
  let quote: Quote = "none";
  let escaped = false;
  let inBacktick = false;
  let parenDepth = 0;

  for (let index = 0; index < command.length; index++) {
    const ch = command[index];
    if (escaped) {
      escaped = false;
      continue;
    }
    if (quote === "single") {
      if (ch === "'") quote = "none";
      continue;
    }
    if (quote === "double") {
      if (ch === "\\") escaped = true;
      else if (ch === '"') quote = "none";
      continue;
    }
    if (inBacktick) {
      if (ch === "`") inBacktick = false;
      continue;
    }
    if (ch === "\\") {
      escaped = true;
      continue;
    }
    if (ch === "'") {
      quote = "single";
      continue;
    }
    if (ch === '"') {
      quote = "double";
      continue;
    }
    if (ch === "`") {
      inBacktick = true;
      continue;
    }
    if (ch === "(") {
      parenDepth++;
      continue;
    }
    if (ch === ")") {
      if (parenDepth > 0) parenDepth--;
      continue;
    }
    if (parenDepth > 0) continue;

    const next = command[index + 1];
    if ((ch === "&" && next === "&") || (ch === "|" && next === "|")) {
      statements.push(command.slice(start, index));
      index++;
      start = index + 1;
    } else if (ch === ";" || ch === "\n" || ch === "&") {
      statements.push(command.slice(start, index));
      start = index + 1;
    }
  }

  if (quote !== "none" || inBacktick || escaped) return null;
  statements.push(command.slice(start));
  return statements;
}

function firstPipelineStage(command: string): string | null {
  let quote: Quote = "none";
  let firstPipeIndex: number | undefined;

  for (let index = 0; index < command.length; index++) {
    const ch = command[index];
    if (quote === "single") {
      if (ch === "'") quote = "none";
      continue;
    }
    if (quote === "double") {
      if (ch === '"') {
        quote = "none";
      } else if (ch === "\\") {
        index++;
      } else if (ch === "`") {
        return null;
      }
      continue;
    }

    if (ch === "'") {
      quote = "single";
    } else if (ch === '"') {
      quote = "double";
    } else if (ch === "\\") {
      index++;
    } else if (ch === "`") {
      return null;
    } else if (ch === "|") {
      if (command[index + 1] === "|") {
        index++;
      } else if (firstPipeIndex === undefined) {
        firstPipeIndex = index;
      }
    }
  }

  if (quote !== "none") return null;
  return command.slice(0, firstPipeIndex ?? command.length).trim();
}

function readShellToken(command: string, start: number): TokenResult | null {
  let quote: Quote = "none";
  let token = "";
  let index = start;

  for (; index < command.length; index++) {
    const ch = command[index];
    if (quote === "single") {
      if (ch === "'") {
        quote = "none";
      } else {
        token += ch;
      }
      continue;
    }
    if (quote === "double") {
      if (ch === '"') {
        quote = "none";
      } else if (ch === "\\") {
        index++;
        token += command[index] ?? "\\";
      } else if (ch === "`") {
        return null;
      } else {
        token += ch;
      }
      continue;
    }

    if (/\s/.test(ch)) break;
    if (isTokenBoundary(ch)) break;
    if (ch === "'") {
      quote = "single";
    } else if (ch === '"') {
      quote = "double";
    } else if (ch === "\\") {
      index++;
      token += command[index] ?? "\\";
    } else if (ch === "`") {
      return null;
    } else {
      token += ch;
    }
  }

  if (quote !== "none") return null;
  return { token, end: index };
}

function isTokenBoundary(ch: string): boolean {
  return ch === "|" || ch === ";" || ch === "&" || ch === "<" || ch === ">";
}

function skipSpaces(input: string, start: number): number {
  let index = start;
  while (index < input.length && /\s/.test(input[index])) index++;
  return index;
}
