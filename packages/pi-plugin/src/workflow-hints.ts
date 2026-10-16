// ---------------------------------------------------------------------------
// Workflow hints — short system prompt block teaching the agent
// token-efficient AFT workflows. Mirrors packages/opencode-plugin/src/workflow-hints.ts;
// the two copies are kept in sync by hand.
// ---------------------------------------------------------------------------

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import type { AftConfig } from "./config.js";
import { resolveBashConfig } from "./config.js";
import { hashlineTagSourceSentence } from "./hashline-tag-sources.js";
import { log } from "./logger.js";
import { skipsEagerStartup } from "./session-kind.js";
import { piHashlineEffective } from "./tool-registration.js";

export interface WorkflowHintsOpts {
  bashBackgroundEnabled: boolean;
  bashCompressionEnabled: boolean;
  /**
   * Resolved `bash.rewrite` flag. Only rewritten `cat`/`head`/`tail` commands
   * mint hashline tags through bash, so the hashline hint names them only when
   * rewriting is on. Omitted means on.
   */
  bashRewriteEnabled?: boolean;
  /** Set of tool names KNOWN-ABSENT from the registered surface. */
  absentTools: Set<string>;
  /** Whether the hashline `edit` arm is the one actually registered. */
  hashlineEffective?: boolean;
}

const HEADING = "## IMPORTANT NOTICE about your tools";

/**
 * Routing rule for hashline sessions: which calls mint the tags a patch
 * addresses (see `hashlineTagSourceSentence`). The bash rewrite path and each
 * AFT navigation tool are named only when present. Kept byte-identical to the
 * OpenCode copy.
 */
export function hashlineTagSourceHint(
  bashRewritesMintTags: boolean,
  isRegistered: (toolName: string) => boolean = () => true,
): string {
  return `**Hashline edit tags**: ${hashlineTagSourceSentence(bashRewritesMintTags, isRegistered)}`;
}

/** The hashline hint for a session whose bash rewrites `cat`/`head`/`tail`. */
export const HASHLINE_TAG_SOURCE_HINT = hashlineTagSourceHint(true);

export function buildWorkflowHints(opts: WorkflowHintsOpts): string | null {
  const sections: string[] = [];

  const grepName = "grep";
  const bashName = "bash";

  const hasOutline = !opts.absentTools.has("aft_outline");
  const hasZoom = !opts.absentTools.has("aft_zoom");
  const readName = "read";
  const hasRead = !opts.absentTools.has(readName);
  const hasGrep = !opts.absentTools.has(grepName);
  const hasSearch = !opts.absentTools.has("aft_search");
  const hasNavigate = !opts.absentTools.has("aft_callgraph");
  const hasInspect = !opts.absentTools.has("aft_inspect");
  const hasBash = !opts.absentTools.has(bashName);
  const hasBgBash = opts.bashBackgroundEnabled && hasBash && !opts.absentTools.has("bash_status");
  // Each background section names its companion, so it needs that tool too.
  const hasWatch = !opts.absentTools.has("bash_watch");
  const hasWrite = !opts.absentTools.has("bash_write");

  if (hasBash && opts.bashCompressionEnabled) {
    // The section itself is config-gated, so the text never hedges with
    // "when compression is on" — the agent can't check the config; we can.
    sections.push(
      [
        "**Test/build output**: bash output is auto-compressed for non-piped commands. Piped commands run verbatim and show the pipeline's output. For AFT's test/build summary, run the runner without filters:",
        "- `bun test | grep fail` → run `bun test`",
        "- `cargo test 2>&1 | tail -20` → run `cargo test`",
        "- `npm run build | head -50` → run `npm run build`",
      ].join("\n"),
    );
  }

  if (hasOutline && hasZoom) {
    sections.push(
      `**Web/URL access**: \`aft_outline({ target: "<url>" })\` first for structure, then \`aft_zoom({ batch: [{ target: "<url>", symbols: "<heading>" }] })\` for the specific section.`,
    );
  }

  // See the OpenCode copy for the rationale — kept byte-identical for parity.
  // Lead imperatively (DO NOT) with the two reflexes agents get wrong:
  // serializing independent lookups, and shelling out to grep for code search.
  // aft_search is named alone when available (it auto-routes literals too);
  // only when absent do we point at the grep TOOL.
  if (hasOutline && (hasGrep || hasSearch) && (hasZoom || hasRead)) {
    const searchName = hasSearch ? "aft_search" : grepName;
    const locate = hasSearch
      ? "`aft_search` is the primary code-search tool: one call auto-routes concepts, identifiers, regex, error strings, and literals."
      : `\`${grepName}\` (the tool — indexed and ranked) locates code.`;
    const zoomSteer = hasZoom ? ", or `aft_zoom`" : "";
    const searchSteer = hasSearch
      ? `use \`aft_search\` (concepts, identifiers, regex, literals), \`${readName}\`, \`aft_outline\`${zoomSteer} instead`
      : `use the \`${grepName}\` tool, \`${readName}\`, \`aft_outline\`${zoomSteer} instead`;
    const bashSteer = hasBash
      ? ` If you are about to run grep, rg, sed, awk, find, or cat through ${bashName} to locate or read code: STOP — ${searchSteer}.`
      : "";
    sections.push(
      [
        `**Code exploration**: ${locate} Then \`aft_outline\` for structure → \`${hasZoom ? "aft_zoom" : readName}\` for symbol(s). DO NOT run \`grep\`/\`rg\`/\`find\`/\`sed\`/\`cat\` through \`bash\` to locate or read code — the bash path is unindexed, unranked, serial, and routinely surfaces the wrong hit. Keep \`bash\` for shell facts (git state, file metadata, processes).${bashSteer} Reflex translations:`,
        `- \`grep -rn "handleAuth" src/\` in bash → \`${searchName}({ query: "handleAuth" })\``,
        `- \`find . -name "*.ts" | xargs grep watcher\` in bash → \`${searchName}({ query: "watcher invalidation" })\` (concepts work too)`,
        `- \`sed -n '100,160p' app.ts\` / \`cat app.ts\` in bash → \`${readName}({ path: "app.ts", startLine: 100, endLine: 160 })\``,
      ].join("\n"),
    );
  }

  if (hasInspect) {
    sections.push(
      "**Codebase health & diagnostics**: AFT does not surface compile/type errors automatically after edits — pull them with `aft_inspect`. Run it after a batch of edits and before you run tests or commit, when starting in unfamiliar code, or before a refactor/review. One call summarizes diagnostics (compile/type errors), TODOs, metrics, dead code, unused exports, and duplicates; pass `sections` for focused drill-down and `scope` to actively pull diagnostics for a specific file or directory. Its diagnostics are a fast checkpoint, not the authority — a clean `tsc` / `cargo check` / `pyright` run is the real gate. Treat stale_categories/pending_categories as stale or incomplete cache state. AFT schedules a Tier-2 refresh after its next idle or inspect-triggered background run; use one later normal aft_inspect after that refresh, not a polling loop.",
    );
    sections.push(
      "**AFT status bar**: tool results may end with a one-line health bar `[AFT E<errors> W<warnings> | D<dead-code> U<unused-exports> C<clone/dup-groups> | T<todos>]` — an IDE-style glance that appears when a count changes. `E`/`W` are live LSP diagnostics for files touched this session (your universal compile-error signal across every language with an LSP). A `~` before `D` means the dead-code/unused/dup counts predate your latest edit — run `aft_inspect` for current numbers and detail. A `?` in place of a number means that count has no value yet (not zero). When `E>0`, you likely just introduced errors; investigate before moving on.",
    );
  }

  if (hasNavigate) {
    sections.push(
      [
        "Use `aft_callgraph` for code-relationship questions instead of grep + read chains:",
        "- `callers` — find all call sites before changing a function signature",
        "- `impact` — blast radius (which functions/files will need updates)",
        "- `trace_to` — how execution reaches this code from entry points (routes, exports, main)",
        "- `trace_to_symbol` — shortest call path from one symbol to another",
        "- `trace_data` — follow a value through assignments and parameters across files",
      ].join("\n"),
    );
  }

  if (hasBgBash && hasWatch) {
    sections.push(
      [
        `**Long-running commands** (builds, installs, full test suites): run them in the FOREGROUND — use \`${bashName}({ command, wait: true })\` when you know it is long and need the result before anything else; if you send a new message, the wait detaches to background; otherwise omit \`wait\` so auto-promote can hand you a reminder while you work.`,
        "- `background: true` is ONLY for when you have OTHER useful work to do while it runs: start it, do the other work, and the completion reminder delivers the result (or spawn a subagent for the side work). Do NOT background a command and then immediately `bash_watch` it — that spends a whole extra turn waiting for something foreground returns in one.",
        "- `bash_watch` is for blocking on an ALREADY-backgrounded task once you've run out of parallel work (sync — the user can interrupt), or reacting to a specific early output line (async: background:true + pattern). Never loop `bash_status` to wait — it's a one-shot inspector.",
      ].join("\n"),
    );
  }
  if (hasBgBash && hasWrite) {
    sections.push(
      `**PTY / interactive commands**: PTY mode is for interactive REPLs and terminal apps (python, node, bash itself, vim). Start with \`${bashName}({ command: "python", pty: true, background: true })\`, read the screen with \`bash_status({ task_id, output_mode: "screen" })\`, and send input with \`bash_write({ task_id, input: "..." })\`.`,
    );
  }

  // Conditional on the hashline arm being the registered one: a legacy-edit
  // session has no tags and must not be told to go mint them.
  if (opts.hashlineEffective === true) {
    sections.push(
      hashlineTagSourceHint(
        hasBash && opts.bashRewriteEnabled !== false,
        (name) => !opts.absentTools.has(name),
      ),
    );
  }

  if (sections.length === 0) {
    return null;
  }

  // The opening notice frames the whole block (parity with OpenCode): these
  // are not ordinary CLI-equivalent tools, and the single biggest efficiency
  // win is firing independent read-only calls together. Prepended so it
  // leads, and only when there's real content below it.
  sections.unshift(
    "You are equipped with a non-standard tool set: indexed code search, symbol-level reading, structural editing, and code analysis that are faster, more precise, and far cheaper in tokens than stitching together command-line utilities in bash. Always reach for these tools first.\n\n**Parallel tool calls**: when several read-only operations are independent, emit them in ONE response instead of serializing — file reads, structure and symbol lookups, code search, diagnostics, and git status/diff/log. Sequence only when a call depends on a prior result or when a command mutates state.",
  );

  return `${HEADING}\n\n${sections.join("\n\n")}`;
}

export function buildHintsFromConfig(
  config: AftConfig,
  absentTools: Set<string>,
  hashlineEffective = false,
): string | null {
  // Background-bash gating reads the resolved bash config so the graduated
  // `bash.background` setting controls whether the hint appears. See
  // `resolveBashConfig` in config.ts.
  const bashCfg = resolveBashConfig(config);
  return buildWorkflowHints({
    bashBackgroundEnabled: bashCfg.background,
    bashCompressionEnabled: bashCfg.compress,
    bashRewriteEnabled: bashCfg.rewrite,
    absentTools,
    hashlineEffective,
  });
}

// ---------------------------------------------------------------------------
// Pi extension registration
// ---------------------------------------------------------------------------

interface ToolSurfaceFlags {
  outline: boolean;
  zoom: boolean;
  semantic: boolean;
  navigate: boolean;
  inspect: boolean;
  hoistGrep: boolean;
  hoistBash: boolean;
  hoistEdit: boolean;
  hoistRead: boolean;
  bashStatus: boolean;
  bashWatch: boolean;
  bashWrite: boolean;
}

/**
 * Name of the prompt section that carries the hints on hosts with structured
 * system prompt sections. Pi renders it as
 * `<aft_workflow_hints>\n…\n</aft_workflow_hints>` and requires names to match
 * `^[a-z][a-z0-9_-]*$`.
 */
export const WORKFLOW_HINTS_SECTION = "aft_workflow_hints";

/**
 * The parts of a `before_agent_start` event the hints handler reads, across
 * the hosts the plugin runs on:
 * - upstream Pi 0.86.0 and later expose `systemPromptOptions.sections`, a
 *   per-run copy that handlers may mutate, and `forceSystemPrompt`, which is
 *   set once any handler has returned `systemPrompt` for this run;
 * - older upstream Pi passes `systemPromptOptions` without `sections` (and it
 *   is the session's base options object, so it must not be mutated);
 * - oh-my-pi (OMP) passes `systemPrompt` as an array with one entry per
 *   provider system block and has no `systemPromptOptions`.
 */
export interface WorkflowHintsStartEvent {
  systemPrompt: string | readonly string[];
  systemPromptOptions?: {
    sections?: Record<string, string>;
    forceSystemPrompt?: string;
  };
}

export type WorkflowHintsStartResult = { systemPrompt: string | string[] } | undefined;

/**
 * Add the hints block to one `before_agent_start` run.
 *
 * Where the host has prompt sections, the block goes into its own section and
 * nothing is returned. Returning `systemPrompt` would force the whole prompt
 * for the run: Pi then sends one collapsed system message and drops the
 * transcript-backed mid-conversation tool and prompt changes, so every later
 * tool activation rewrites the whole prompt cache. As a section, the block is
 * diffed like any other part of the prompt and, being identical on every run,
 * is sent once and stays in the cached prefix.
 *
 * Hosts without sections only accept a returned replacement prompt, so the
 * block is appended to the prompt they passed in: as a new last entry when the
 * prompt is an array of system blocks (OMP), keeping the existing blocks
 * intact, or after a blank line when it is a single string (older Pi).
 */
export function applyWorkflowHints(
  event: WorkflowHintsStartEvent,
  hintsBlock: string,
): WorkflowHintsStartResult {
  const options = event.systemPromptOptions;
  const sections = options?.sections;
  if (sections !== null && typeof sections === "object") {
    sections[WORKFLOW_HINTS_SECTION] = hintsBlock;
    // An earlier handler already forced this run's prompt, and Pi sends forced
    // text exactly as given, so the section above would not reach the model.
    // Append to the forced text instead; the run is forced either way, so this
    // costs nothing extra. Keeping the section set as well means the recorded
    // transcript sections do not change between forced and unforced runs.
    if (options?.forceSystemPrompt !== undefined) {
      return { systemPrompt: `${event.systemPrompt}\n\n${hintsBlock}` };
    }
    return undefined;
  }
  if (Array.isArray(event.systemPrompt)) {
    return { systemPrompt: [...event.systemPrompt, hintsBlock] };
  }
  return { systemPrompt: `${event.systemPrompt}\n\n${hintsBlock}` };
}

/**
 * Register the workflow-hints extension on Pi via `before_agent_start`.
 *
 * Pi fires `before_agent_start` once per user prompt with the prompt it
 * assembled; {@link applyWorkflowHints} adds the hints block to it without
 * dropping other extensions' contributions. The block is built once here, so
 * it is byte-identical on every run of the session.
 * Magic Context's short-lived Pi children don't need these workflow hints.
 */
export function registerWorkflowHints(
  pi: ExtensionAPI,
  config: AftConfig,
  surface: ToolSurfaceFlags,
): void {
  if (skipsEagerStartup()) return;

  // Build the absent-tools set from the resolved registration predicates.
  const absent = new Set<string>();
  if (!surface.outline) absent.add("aft_outline");
  if (!surface.zoom) absent.add("aft_zoom");
  if (!surface.semantic) absent.add("aft_search");
  if (!surface.navigate) absent.add("aft_callgraph");
  if (!surface.inspect) absent.add("aft_inspect");
  if (!surface.hoistGrep) absent.add("grep");
  if (!surface.hoistRead) absent.add("read");
  if (!surface.hoistBash) absent.add("bash");
  if (!surface.bashStatus) absent.add("bash_status");
  if (!surface.bashWatch) absent.add("bash_watch");
  if (!surface.bashWrite) absent.add("bash_write");

  const hintsBlock = buildHintsFromConfig(config, absent, piHashlineEffective(config, surface));
  if (!hintsBlock) return;

  log(`Workflow hints injected (${hintsBlock.length} chars)`);

  // The installed Pi typings predate prompt sections and OMP's array prompt,
  // so the handler is registered against the cross-host event shape.
  (
    pi.on as (
      event: "before_agent_start",
      handler: (event: WorkflowHintsStartEvent) => WorkflowHintsStartResult,
    ) => void
  )("before_agent_start", (event) => applyWorkflowHints(event, hintsBlock));
}
