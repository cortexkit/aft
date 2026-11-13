import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import { isTerminalStatus } from "./bash-format.js";
import { relativePathEscapesRoot } from "./path-display.js";

// Helpers for the bash-output hint nudges appended to bash tool results.
//
// Shared across harnesses (OpenCode applies it in `tool.execute.after`; Pi
// applies it inside its hoisted bash tool). Returns the new output string (or
// the original when no hint should fire). The appended "[Hint] ..." line is
// agent-visible and persists in the tool result.

/** Who is calling bash_watch: a delegated worker session or a primary one. */
export type WatchCallerRole = "worker" | "primary";

/** Sync bash_watch deadline used by a primary session that passes no timeout. */
export const DEFAULT_PRIMARY_WATCH_TIMEOUT_MS = 30_000;

/** Largest bash_watch timeout accepted by the OpenCode and Pi plugins' tool schemas. */
export const MAX_WATCH_TIMEOUT_MS = 1_800_000;

/**
 * Default `bash.worker_wait_max_ms`: the longest a delegated worker's wait on
 * one command blocks before control returns to it. Mirrors
 * `DEFAULT_BASH_WORKER_WAIT_MAX_MS` in the engine's `config.rs`. A worker that
 * blocked with no limit once sat behind a stuck test run for fifteen hours;
 * with the limit it gets control back, sees the command is still running, and
 * decides whether to wait again or kill it.
 */
export const DEFAULT_WORKER_WAIT_MAX_MS = 1_800_000;

/** Smallest accepted `bash.worker_wait_max_ms`; mirrors `MIN_BASH_WORKER_WAIT_MAX_MS`. */
export const MIN_WORKER_WAIT_MAX_MS = 60_000;

/**
 * Longest delay a JavaScript timer accepts (2^31 - 1 ms, about 24.8 days); a
 * larger one fires almost at once. Bridge transport timeouts derived from a
 * configured limit are clamped to it.
 */
export const LONGEST_TIMER_DELAY_MS = 2_147_483_647;

/**
 * How a delegated worker's waits are bounded, worded once for every tool
 * description. It names the setting, not its value, so the prompt prefix is
 * the same for every user.
 */
export const WORKER_WAIT_LIMIT_PHRASE =
  "the worker wait limit (`bash.worker_wait_max_ms`, 30 minutes by default)";

/**
 * Description of the bash_watch tool's sync-wait defaults, embedded in both
 * plugins' tool descriptions. Every caller sees the same description, so it
 * has to state both roles' real behaviour.
 */
export const WATCH_SYNC_DEFAULTS_DESCRIPTION =
  "Sync waits default to 30s in a main session (max `bash.watch_sync_max_ms`, 120s by default); in a delegated session a sync wait without a timeout waits up to the worker wait limit (`bash.worker_wait_max_ms`, 30 minutes by default), then reports it is still running; watch again to keep waiting. An explicit timeout never exceeds that worker limit";

/**
 * Description of the bash_watch timeout parameter. Every caller sees the same
 * schema, so it has to be true for both roles.
 */
export const WATCH_TIMEOUT_PARAM_DESCRIPTION =
  "Sync-only timeout in milliseconds. In a main session: default 30000, max `bash.watch_sync_max_ms` (120000 by default). In a delegated session: omit it to wait up to the worker wait limit (`bash.worker_wait_max_ms`, 30 minutes by default), after which the watch reports the command is still running; a value you pass is clamped to that worker limit.";

/**
 * Effective sync bash_watch deadline in milliseconds.
 *
 * A delegated worker cannot be woken once its turn ends, so it has nothing
 * useful to do while its command runs except wait, and a short deadline only
 * makes it call bash_watch again, re-reading its whole context each time. It
 * therefore waits up to the worker wait limit (`workerWaitMaxMs`,
 * `bash.worker_wait_max_ms`) by default: long enough that re-watching is
 * rare, short enough that a stuck command cannot hold it forever. A timeout
 * it passes is clamped to the worker limit, not to the primary cap. A
 * primary keeps the short default and the cap because it can do other work
 * or end its turn and be woken by the completion reminder.
 */
export function resolveWatchTimeoutMs(
  requestedMs: number | undefined,
  role: WatchCallerRole,
  capMs: number,
  workerWaitMaxMs: number,
): number {
  if (role === "worker") return Math.min(requestedMs ?? workerWaitMaxMs, workerWaitMaxMs);
  return Math.min(requestedMs ?? DEFAULT_PRIMARY_WATCH_TIMEOUT_MS, capMs);
}

/**
 * Largest bash_watch timeout a caller of this role may pass. A primary is
 * bounded by the configured cap; a worker only by the schema maximum.
 */
export function maxWatchTimeoutMs(role: WatchCallerRole, capMs: number): number {
  return role === "worker" ? MAX_WATCH_TIMEOUT_MS : capMs;
}

/**
 * Delay before the next bash_watch status poll. The first seconds poll fast so
 * a short command's exit or a quick pattern is seen promptly; after that the
 * interval grows, because a wait that can last as long as a build would
 * otherwise send ten status requests a second for its whole length. The
 * longest interval is also the worst-case delay before a new message or an
 * abort ends the wait.
 */
export function watchPollDelayMs(elapsedMs: number): number {
  if (elapsedMs < 5_000) return 100;
  if (elapsedMs < 30_000) return 250;
  if (elapsedMs < 120_000) return 500;
  return 1_000;
}

/**
 * Clock and sleep used by the bash_watch wait loops. Production uses the
 * suspend-inclusive elapsed clock and a real timer; tests replace both fields so a wait that
 * lasts minutes of simulated time runs in milliseconds.
 */
const waitClock = new WaitClock();

export const watchClock: {
  now: () => number;
  sleep: (ms: number, signal?: AbortSignal) => Promise<void>;
} = {
  now: () => waitClock.now(),
  sleep: abortableSleep,
};

/** Sleep that resolves early when `signal` aborts, so an aborted wait ends at once. */
export function abortableSleep(ms: number, signal?: AbortSignal): Promise<void> {
  if (signal?.aborted) return Promise.resolve();
  return new Promise((resolve) => {
    const done = () => {
      clearTimeout(timer);
      signal?.removeEventListener("abort", done);
      resolve();
    };
    const timer = setTimeout(done, ms);
    signal?.addEventListener("abort", done, { once: true });
  });
}

/**
 * Opening of every sync bash_watch reply line: the real time the watch held
 * the call, next to the limit it was allowed. Printing both lets a reader see
 * at a glance whether the watch ran to its limit or returned early, and when
 * the limit is the configured cap it names the knob, so a caller that wanted
 * longer knows what bounded it. `limitMs` is undefined for a wait with no
 * deadline, and `capMs` is undefined when the caller is not bounded by the
 * configured cap (a delegated worker). `elapsedMs` must come from a monotonic
 * clock (see monotonicNowMs).
 */
export function formatWatchWaited(
  elapsedMs: number,
  limitMs: number | undefined,
  capMs: number | undefined,
): string {
  let limit: string;
  if (limitMs === undefined) limit = "no wait limit";
  else if (capMs !== undefined && limitMs >= capMs)
    limit = `limit ${limitMs}ms, the bash.watch_sync_max_ms cap`;
  else limit = `limit ${limitMs}ms`;
  return `Waited ${Math.round(elapsedMs)}ms (${limit})`;
}

/**
 * Appended to a primary's bash_watch reply whose sync deadline passed without
 * a match. The deadline is a property of the watch, not of the command, so the
 * caller is told the command is still running and that the completion
 * reminder wakes it. A worker gets {@link workerWatchStillRunning} instead.
 */
export function watchTimeoutSteer(): string {
  return "The command is still running; this is not a failure. Watch again, do other work, or end your turn: the completion reminder wakes you.";
}

/** A duration in words: whole minutes as minutes, anything else in seconds. */
export function formatWaitDuration(ms: number): string {
  const rounded = Math.max(0, Math.round(ms));
  if (rounded === 60_000) return "1 minute";
  if (rounded > 0 && rounded % 60_000 === 0) return `${rounded / 60_000} minutes`;
  const seconds = (rounded / 1000).toFixed(1).replace(/\.0$/, "");
  return `${seconds}s`;
}

/**
 * The sentence naming a task's own kill deadline, from the `hard_kill` field
 * and `started_at` of the engine's task status (`{ limit_ms, source }`), or how
 * the task was killed when its hard kill fired (`status_reason`). It reports an
 * absolute UTC deadline measured from command start and the approximate time
 * remaining. Mirrors `kill_deadline_sentence` in the engine.
 */
export function taskKillDeadlineText(
  data: Record<string, unknown>,
  role: WatchCallerRole,
  nowMs = Date.now(),
): string {
  if (data.status === "timed_out") {
    const reason = typeof data.status_reason === "string" ? data.status_reason : "";
    return reason === ""
      ? "The task was killed by its time limit (exit 124)."
      : `The task was ${reason}.`;
  }
  // A terminal task's deadline no longer matters; an unknown state (the
  // bridge stayed busy) has none to report.
  if (isTerminalStatus(data.status) || typeof data.status !== "string") return "";
  if (data.status === "unknown") return "";
  // A remote task has no AFT hard-kill deadline (`hard_kill`); report its
  // build-server status note instead (still queued, or when the runner will
  // stop it). Mirrors `kill_deadline_text` in crates/aft/src/commands/bash_watch.rs.
  if (typeof data.remote_phase_note === "string" && data.remote_phase_note !== "") {
    const note = data.remote_phase_note;
    return `${note.charAt(0).toUpperCase()}${note.slice(1)}${note.endsWith(".") ? "" : "."}`;
  }
  const hardKill = data.hard_kill as { limit_ms?: unknown; source?: unknown } | undefined;
  if (!hardKill || typeof hardKill.limit_ms !== "number") {
    return "This task has no kill deadline.";
  }
  const startedAtMs = data.started_at;
  if (typeof startedAtMs !== "number" || !Number.isSafeInteger(startedAtMs)) {
    return "This task has no kill deadline.";
  }
  const limitMs = hardKill.limit_ms;
  const limit = formatWaitDuration(limitMs);
  const deadlineAt = startedAtMs + limitMs;
  const when = `at ${formatKillDeadlineUtc(deadlineAt)}, when it has run ${limit}`;
  let source: string;
  if (hardKill.source === "timeout") {
    source = "(the `timeout` you passed)";
  } else if (role === "worker") {
    source =
      "(its default background limit), but each wait you make on it moves that kill to at least the worker wait limit (`bash.worker_wait_max_ms`) after the wait, so it is not killed while you keep waiting; pass a `timeout` to set your own limit";
  } else {
    source = "(its default background limit) unless you pass a longer `timeout`";
  }
  const remaining =
    deadlineAt <= nowMs
      ? "; the kill deadline has passed"
      : `; about ${formatApproximateRemaining(deadlineAt - nowMs)} remain`;
  return `AFT kills this task ${when} ${source}${remaining}.`;
}

/**
 * A wait should not hand back a still-running task just before its own kill
 * deadline. Five seconds covers the slowest status poll and kill publication.
 */
export function taskKillDeadlineWithinHandoffMargin(data: Record<string, unknown>): boolean {
  if (typeof data.status !== "string" || isTerminalStatus(data.status)) return false;
  const hardKill = data.hard_kill as { limit_ms?: unknown } | undefined;
  const limitMs = hardKill?.limit_ms;
  const elapsedMs = data.elapsed_ms;
  return (
    typeof limitMs === "number" &&
    Number.isSafeInteger(limitMs) &&
    typeof elapsedMs === "number" &&
    Number.isSafeInteger(elapsedMs) &&
    limitMs - elapsedMs <= 5_000
  );
}

function formatKillDeadlineUtc(unixMs: number): string {
  const date = new Date(unixMs);
  if (!Number.isFinite(date.getTime())) throw new Error("invalid bash task start time");
  const year = String(date.getUTCFullYear()).padStart(4, "0");
  const month = String(date.getUTCMonth() + 1).padStart(2, "0");
  const day = String(date.getUTCDate()).padStart(2, "0");
  const hour = String(date.getUTCHours()).padStart(2, "0");
  const minute = String(date.getUTCMinutes()).padStart(2, "0");
  const second = String(date.getUTCSeconds()).padStart(2, "0");
  const millis = date.getUTCMilliseconds();
  const fraction = millis === 0 ? "" : `.${String(millis).padStart(3, "0")}`;
  return `${year}-${month}-${day} ${hour}:${minute}:${second}${fraction}Z`;
}

function formatApproximateRemaining(ms: number): string {
  if (ms >= 60_000) {
    const minutes = Math.max(1, Math.floor((ms + 30_000) / 60_000));
    return `${minutes} minute${minutes === 1 ? "" : "s"}`;
  }
  return formatWaitDuration(ms);
}

/** Most lines of a still-running command's output shown to a worker. */
const WORKER_OUTPUT_TAIL_LINES = 20;

/** The last lines of `output`, so a worker can judge whether a command is stuck. */
export function outputTail(output: string | undefined): string {
  const lines = (output ?? "").replace(/\s+$/, "").split("\n");
  return lines.slice(-WORKER_OUTPUT_TAIL_LINES).join("\n");
}

/**
 * Appended to a delegated worker's bash_watch reply whose deadline passed
 * while the command still runs (by default the worker wait limit). The
 * deadline is a property of the watch, not of the command: a worker that read
 * a bare "timeout reached" line as its own execution failing declared a
 * failed result while its command was still going. So it is told plainly that
 * the command is still running, how long it has run, and its latest output,
 * then given both moves by name: watch again to keep waiting, or kill a
 * command that should have finished. `taskIdArg` and `timeoutParam` are the
 * host's spellings of the bash_watch arguments.
 */
export function workerWatchStillRunning(options: {
  taskId: string;
  waitedMs: number;
  ranMs: number | undefined;
  output: string | undefined;
  taskIdArg?: string;
  timeoutParam?: string;
}): string {
  const taskIdArg = options.taskIdArg ?? "taskId";
  const timeoutParam = options.timeoutParam ?? "timeoutMs";
  const ran =
    options.ranMs === undefined ? "" : ` It has run for ${formatWaitDuration(options.ranMs)}.`;
  const tail = outputTail(options.output);
  const output = tail === "" ? "No output yet." : `Recent output:\n${tail}`;
  return `The command is still running after ${formatWaitDuration(options.waitedMs)} of watching; this is not a failure.${ran} Call bash_watch({ ${taskIdArg}: "${options.taskId}" }) again to keep waiting (without ${timeoutParam} a watch waits up to the worker wait limit, then reports it is still running), or bash_kill({ ${taskIdArg}: "${options.taskId}" }) if it should have finished by now. Don't report a result until it finishes.\n${output}`;
}

/**
 * How long a sync bash_watch with no deadline keeps retrying while every
 * status poll times out because the bridge is busy. Without this bound a wait
 * with no deadline would retry a wedged bridge forever; after it, the watch
 * returns and says the task state is unknown.
 */
export const WATCH_UNAVAILABLE_GIVE_UP_MS = 120_000;

/**
 * What a delegated worker is told whenever its watch returned while its task
 * is still running. A worker cannot be woken after its turn ends, so the only
 * right move is to wait again.
 */
export const WORKER_KEEP_WAITING =
  "Call bash_watch again to keep waiting; don't report a result until the command finishes.";

/**
 * Tail of a sync bash_watch reply interrupted by a new message, saying what
 * happens to the still-running task next. A primary is woken later by the
 * completion reminder or an async watch, as `primaryTail` says; a worker
 * cannot be woken once its turn ends, so it is told to wait again instead.
 */
export function interruptedWatchTail(role: WatchCallerRole, primaryTail: string): string {
  return role === "worker" ? `The task is still running. ${WORKER_KEEP_WAITING}` : primaryTail;
}

/**
 * Appended to a bash reply shown to a delegated worker (subagent) whose
 * command now runs in the background. AFT's own reply already says the task
 * won't wake the worker; this names the plugin-owned bash_watch tool, which
 * AFT cannot assume a host has. The suggested call passes no timeout on
 * purpose: a worker's watch without one waits up to the worker wait limit,
 * the longest it may, so any number would only make it wake sooner and watch
 * again. `taskIdArg` is the host's spelling of the task id argument.
 */
export function workerBackgroundTaskNote(taskId: string, taskIdArg = "taskId"): string {
  return `\n\nNOTE (subagent session): Continue with other work if you have it. If you don't, call bash_watch({ ${taskIdArg}: "${taskId}" }) to wait for completion before returning to the parent; without a timeout it waits up to ${WORKER_WAIT_LIMIT_PHRASE}, then reports it is still running; watch again to keep waiting, or call bash_kill({ ${taskIdArg}: "${taskId}" }) if it should have finished by now. Subagents don't survive turn-end and won't be woken when the command finishes.`;
}

/**
 * Line a bash_status snapshot of a still-running task ends with. A primary is
 * woken by the completion reminder; a delegated worker is not, so it is
 * pointed at bash_watch instead.
 */
export function runningTaskStatusHint(role: WatchCallerRole): string {
  return role === "worker"
    ? "To wait for it, call bash_watch; don't poll."
    : "A completion reminder will be delivered automatically; don't poll.";
}

/**
 * Tail of a sync bash_watch reply that ended because the bridge stayed busy,
 * so the task's state is unknown. Only a primary can rely on the completion
 * notification to wake it.
 */
export function watchUnavailableSteer(role: WatchCallerRole): string {
  const unknown = "the bridge was busy, so task state is unknown.";
  if (role === "worker") return `${unknown} ${WORKER_KEEP_WAITING}`;
  return `${unknown} Do not poll; let the task's completion notification wake the session, or use one bash_status snapshot on the next normal tool call.`;
}

const CONFLICT_HINT =
  "\n\n[Hint] Use aft_conflicts to see all conflict regions across files in a single call.";

const GREP_SEARCH_AFT_SEARCH_HINT =
  "DO NOT search code by running grep/rg in bash — it is unindexed, unranked, and serial. Use the `aft_search` tool instead (it auto-routes concepts, identifiers, regex, and literals).";

const GREP_SEARCH_GREP_HINT =
  "DO NOT search code by running grep/rg in bash — it is unindexed, unranked, and serial. Use the `grep` tool instead (indexed and ranked).";

const GREP_SEARCH_HINT_PREFIX = "DO NOT search code by running grep/rg in bash —";
const GREP_SEARCH_FRESHNESS_WINDOW_MS = 60_000;
const MTIME_CLOCK_SLACK_MS = 1;

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
export function commandInvokesCodeSearch(
  command: string,
  platform: NodeJS.Platform = process.platform,
): boolean {
  const statements = splitTopLevelStatements(command, platform);
  if (statements === null) return false;

  for (const statement of statements) {
    const firstStage = firstPipelineStage(statement, platform);
    if (firstStage === null) continue;
    const firstToken = readShellToken(firstStage, skipSpaces(firstStage, 0), platform);
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
  platform: NodeJS.Platform = process.platform,
  workdir?: string,
): string {
  if (output === "") return output;
  if (!commandInvokesCodeSearch(command, platform)) return output;
  if (output.includes(GREP_SEARCH_HINT_PREFIX)) return output;
  if (shouldSuppressGrepSearchHint(command, projectRoot, platform, workdir)) return output;

  const hint = aftSearchRegistered ? GREP_SEARCH_AFT_SEARCH_HINT : GREP_SEARCH_GREP_HINT;
  return `${output}\n\n${hint}`;
}

function shouldSuppressGrepSearchHint(
  command: string,
  projectRoot: string | undefined,
  platform: NodeJS.Platform,
  workdir: string | undefined,
): boolean {
  const statements = splitTopLevelStatements(command, platform);
  if (statements === null) return false;

  if (allSearchPathOperandsAreDynamic(statements, platform)) return true;

  const root = projectRoot?.trim();
  if (!root) return false;

  const paths = platform === "win32" ? path.win32 : path.posix;
  const resolvedRoot = paths.resolve(root);
  // Track the effective cwd across top-level statements so a `cd` into another
  // repo before the grep is honored: aft_search only indexes THIS project, so a
  // grep that runs outside the project root must NOT be nudged toward it. `null`
  // means the cwd became unknown (dynamic/`cd -`) and we can't confirm the grep
  // is in-project — treat such greps as external (suppress).
  // Explicit workdirs resolve against the indexed project, just as execution
  // does. Keep that project boundary separate from the command's starting cwd.
  let effectiveCwd: string | null = paths.resolve(resolvedRoot, workdir ?? ".");
  let sawCodeSearchStatement = false;

  for (const statement of statements) {
    const firstStage = firstPipelineStage(statement, platform);
    if (firstStage === null) continue;
    const firstToken = readShellToken(firstStage, skipSpaces(firstStage, 0), platform);
    if (firstToken === null) continue;
    const head = firstToken.token;

    if (head === "cd") {
      effectiveCwd = nextCwdAfterCd(effectiveCwd, firstStage, firstToken.end, platform);
      continue;
    }

    if (head !== "grep" && head !== "rg") continue;
    sawCodeSearchStatement = true;

    // A grep whose effective cwd is unknown or outside the project is external —
    // don't fire for it. Keep scanning in case a later statement greps in-project.
    if (effectiveCwd === null) continue;
    if (!isDirInsideProject(resolvedRoot, effectiveCwd, platform)) continue;

    const operands = collectPathOperands(firstStage, firstToken.end, platform);
    // No path operands → grep reads stdin or searches the (in-project) cwd.
    if (operands.length === 0) return false;
    let sawInProjectOperand = false;
    for (const operand of operands) {
      if (isDynamicPathOperand(operand)) continue;
      const resolvedOperand = resolvePathOperand(effectiveCwd, operand, platform);
      if (!isPathInsideProject(resolvedRoot, effectiveCwd, operand, platform)) continue;
      const suppress = shouldSuppressResolvedPath(resolvedOperand);
      if (suppress === undefined) continue; // A missing operand cannot confirm an in-project search.
      sawInProjectOperand = true;
      if (suppress) return true;
    }
    // No existing in-project operands → this grep cannot confirm an in-project search.
    if (sawInProjectOperand) return false;
  }

  return sawCodeSearchStatement;
}

function allSearchPathOperandsAreDynamic(statements: string[], platform: NodeJS.Platform): boolean {
  let sawDynamicOnlySearch = false;
  for (const statement of statements) {
    const firstStage = firstPipelineStage(statement, platform);
    if (firstStage === null) continue;
    const firstToken = readShellToken(firstStage, skipSpaces(firstStage, 0), platform);
    if (firstToken === null) continue;
    const head = firstToken.token;
    if (head !== "grep" && head !== "rg") continue;

    const operands = collectPathOperands(firstStage, firstToken.end, platform);
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
  platform: NodeJS.Platform,
): string | null {
  const paths = platform === "win32" ? path.win32 : path.posix;
  let index = skipSpaces(firstStage, afterCd);
  let target: string | null = null;
  while (index < firstStage.length) {
    const tokenResult = readShellToken(firstStage, index, platform);
    if (tokenResult === null) break;
    const { token, end } = tokenResult;
    if (end <= index) break; // parked on an operator boundary
    index = skipSpaces(firstStage, end);
    if (token === "-") return null; // `cd -` → previous dir, unknown
    if (token.length > 1 && token.startsWith("-")) continue; // a flag like -P/-L
    target = token;
    break;
  }
  if (target === null) return paths.resolve(os.homedir()); // bare `cd` → home
  if (target.includes("$") || target.includes("`")) return null; // dynamic
  const expanded = expandTilde(target, platform);
  if (paths.isAbsolute(expanded)) return paths.resolve(expanded);
  if (currentCwd === null) return null;
  return paths.resolve(currentCwd, expanded);
}

function isDirInsideProject(resolvedRoot: string, dir: string, platform: NodeJS.Platform): boolean {
  const paths = platform === "win32" ? path.win32 : path.posix;
  const rel = paths.relative(resolvedRoot, paths.resolve(dir));
  return !relativePathEscapesRoot(rel, platform);
}

function collectPathOperands(
  firstStage: string,
  startAfterCommand: number,
  platform: NodeJS.Platform,
): string[] {
  const operands: string[] = [];
  let index = skipSpaces(firstStage, startAfterCommand);
  // grep's first positional operand is the pattern, not a path (`grep PAT
  // file`); skipping it lets a dynamic token after it (`grep PAT $LOG`) count
  // as a path operand. A `$VAR`/backtick token carries no slash, so without this
  // the grep read as path-less ("in-project cwd") and nudged toward aft_search
  // for a target it could not see - the logs directory, in the field.
  let sawPattern = false;
  let optionsEnded = false;
  let optionValue: "pattern" | "file" | "other" | undefined;

  while (index < firstStage.length) {
    const tokenResult = readShellToken(firstStage, index, platform);
    if (tokenResult === null) break;
    const { token, end } = tokenResult;
    // No forward progress means readShellToken is parked on a redirection/
    // operator boundary char (`<`, `>`, `|`, `;`, `&`) that it returns as an
    // empty token without advancing. Stop here: grep's search-path operands
    // precede any redirection, and continuing would spin forever (e.g.
    // `grep p f 2>/dev/null` parks on `>`). This guarantees loop termination.
    if (end <= index) break;
    index = skipSpaces(firstStage, end);
    // A descriptor before a redirection is shell syntax, not a search path.
    if (/^\d+$/.test(token) && /[<>]/.test(firstStage[end] ?? "")) break;

    if (optionValue) {
      if (optionValue === "pattern" || optionValue === "file") sawPattern = true;
      optionValue = undefined;
      continue;
    }
    if (!optionsEnded && token === "--") {
      optionsEnded = true;
      continue;
    }
    if (!optionsEnded && token.startsWith("-")) {
      if (token === "-e" || token === "--regexp") optionValue = "pattern";
      else if (token === "-f" || token === "--file") optionValue = "file";
      else if (/^(?:--regexp=|--file=|-e.+|-f.+)/.test(token)) sawPattern = true;
      else if (
        [
          "-A",
          "-B",
          "-C",
          "-m",
          "--after-context",
          "--before-context",
          "--context",
          "--max-count",
          "--include",
          "--exclude",
          "--exclude-dir",
          "-g",
          "--glob",
          "-t",
          "--type",
          "-T",
          "--type-not",
        ].includes(token)
      )
        optionValue = "other";
      continue;
    }
    if (!sawPattern) {
      sawPattern = true;
      continue;
    }
    operands.push(token);
  }

  return operands;
}

function isDynamicPathOperand(token: string): boolean {
  return token.startsWith("~") || token.includes("$") || token.includes("`");
}

function expandTilde(target: string, platform: NodeJS.Platform): string {
  if (!target.startsWith("~")) return target;
  if (target === "~" || target.startsWith("~/")) {
    const paths = platform === "win32" ? path.win32 : path.posix;
    return paths.join(os.homedir(), target.slice(1));
  }
  return target;
}

function resolvePathOperand(
  projectRoot: string,
  operand: string,
  platform: NodeJS.Platform,
): string {
  const paths = platform === "win32" ? path.win32 : path.posix;
  const expanded = expandTilde(operand, platform);
  return paths.isAbsolute(expanded)
    ? paths.resolve(expanded)
    : paths.resolve(projectRoot, expanded);
}

function isPathInsideProject(
  resolvedRoot: string,
  baseCwd: string,
  operand: string,
  platform: NodeJS.Platform,
): boolean {
  // Relative operands resolve against the grep's effective cwd (which may differ
  // from the project root after a `cd`), absolute/`~` operands against the FS.
  const paths = platform === "win32" ? path.win32 : path.posix;
  const resolved = resolvePathOperand(baseCwd, operand, platform);
  const rel = paths.relative(resolvedRoot, resolved);
  return !relativePathEscapesRoot(rel, platform);
}

function shouldSuppressResolvedPath(resolved: string): boolean | undefined {
  try {
    const stats = fs.statSync(resolved);
    if (stats.isFile()) return true;
    // mtimeMs keeps sub-millisecond precision while Date.now() is whole
    // milliseconds, so a directory changed in this same millisecond reads as
    // slightly in the future. Allow that much; a later mtime is not fresh.
    const ageMs = Date.now() - stats.mtimeMs;
    return ageMs > -MTIME_CLOCK_SLACK_MS && ageMs < GREP_SEARCH_FRESHNESS_WINDOW_MS;
  } catch {
    return undefined;
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
function splitTopLevelStatements(
  command: string,
  platform: NodeJS.Platform = process.platform,
): string[] | null {
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
      if (ch === "\\" && backslashEscapesNext(command[index + 1], quote, platform)) escaped = true;
      else if (ch === '"') quote = "none";
      continue;
    }
    if (inBacktick) {
      if (ch === "`") inBacktick = false;
      continue;
    }
    if (ch === "\\" && backslashEscapesNext(command[index + 1], quote, platform)) {
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

function firstPipelineStage(
  command: string,
  platform: NodeJS.Platform = process.platform,
): string | null {
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
      } else if (ch === "\\" && backslashEscapesNext(command[index + 1], quote, platform)) {
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
    } else if (ch === "\\" && backslashEscapesNext(command[index + 1], quote, platform)) {
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

export function readShellToken(
  command: string,
  start: number,
  platform: NodeJS.Platform = process.platform,
): TokenResult | null {
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
      } else if (ch === "\\" && backslashEscapesNext(command[index + 1], quote, platform)) {
        index++;
        if (command[index] !== "\n") token += command[index];
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
    } else if (ch === "\\" && backslashEscapesNext(command[index + 1], quote, platform)) {
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

function backslashEscapesNext(
  next: string | undefined,
  quote: Quote,
  platform: NodeJS.Platform,
): boolean {
  // Bash only unescapes these characters inside double quotes. Outside quotes,
  // Windows paths keep their separators because PowerShell escapes with a backtick.
  if (quote === "double") return next !== undefined && '$`"\\\n'.includes(next);
  return platform !== "win32";
}

function isTokenBoundary(ch: string): boolean {
  return ch === "|" || ch === ";" || ch === "&" || ch === "<" || ch === ">";
}

function skipSpaces(input: string, start: number): number {
  let index = start;
  while (index < input.length && /\s/.test(input[index])) index++;
  return index;
}

import { WaitClock } from "./wait-clock.js";
