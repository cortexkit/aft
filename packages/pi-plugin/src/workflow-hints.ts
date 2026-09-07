// ---------------------------------------------------------------------------
// Workflow hints — short system prompt block teaching the agent
// token-efficient AFT workflows. Mirrors packages/opencode-plugin/src/workflow-hints.ts;
// the two copies are kept in sync by hand.
// ---------------------------------------------------------------------------

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import type { AftConfig } from "./config.js";
import { resolveBashConfig } from "./config.js";
import { log } from "./logger.js";
import { piHashlineEffective } from "./tool-registration.js";

export interface WorkflowHintsOpts {
  toolSurface: "minimal" | "recommended" | "all";
  hoistBuiltins: boolean;
  semanticEnabled: boolean;
  bashBackgroundEnabled: boolean;
  bashCompressionEnabled: boolean;
  /** Set of tool names KNOWN-ABSENT from the registered surface. */
  absentTools: Set<string>;
  /** Whether the hashline `edit` arm is the one actually registered. */
  hashlineEffective?: boolean;
}

const HEADING = "## IMPORTANT NOTICE about your tools";

/**
 * Routing rule for hashline sessions. Kept byte-identical to the OpenCode copy.
 *
 * Navigation tools return source that looks edit-ready but never publishes a
 * snapshot, so an agent that inspects a symbol and then patches it is refused
 * for a tag it believes it already has.
 */
export const HASHLINE_TAG_SOURCE_HINT =
  "**Hashline edit tags**: Only `read` (and accepted AFT `cat`/`head`/`tail` rewrites) mint hashline tags. `aft_zoom`, `aft_outline`, `grep`, `aft_search`, and conflict snippets do not. After navigation, call `read` on every file and range the patch addresses.";

export function buildWorkflowHints(opts: WorkflowHintsOpts): string | null {
  const sections: string[] = [];

  // Built-in replacements use host names by default and aft_ alternatives
  // when hoisting is disabled. Background controls remain unprefixed because
  // they address AFT-owned task IDs rather than host-native bash tasks.
  const grepName = opts.hoistBuiltins ? "grep" : "aft_grep";
  const bashName = opts.hoistBuiltins ? "bash" : "aft_bash";

  const hasOutline = !opts.absentTools.has("aft_outline");
  const hasZoom = !opts.absentTools.has("aft_zoom");
  const readName = opts.hoistBuiltins ? "read" : "aft_read";
  const hasRead = !opts.absentTools.has(readName);
  const hasGrep = opts.toolSurface !== "minimal" && !opts.absentTools.has(grepName);
  const hasSearch =
    opts.toolSurface !== "minimal" && opts.semanticEnabled && !opts.absentTools.has("aft_search");
  const hasNavigate = opts.toolSurface === "all" && !opts.absentTools.has("aft_callgraph");
  const hasInspect = opts.toolSurface !== "minimal" && !opts.absentTools.has("aft_inspect");
  const hasBash = !opts.absentTools.has(bashName);
  const hasBgBash = opts.bashBackgroundEnabled && hasBash && !opts.absentTools.has("bash_status");

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
      `**Web/URL access**: \`aft_outline({ target: "<url>" })\` first for structure, then \`aft_zoom({ url: "<url>", symbols: "<heading>" })\` for the specific section.`,
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
      "**AFT status bar**: tool results may end with a one-line health bar `[AFT E<errors> W<warnings> | D<dead-code> U<unused-exports> C<clone/dup-groups> | T<todos>]` — an IDE-style glance that appears when a count changes. `E`/`W` are live LSP diagnostics for files touched this session (your universal compile-error signal across every language with an LSP). A `~` before `D` means the dead-code/unused/dup counts predate your latest edit — run `aft_inspect` for current numbers and detail. When `E>0`, you likely just introduced errors; investigate before moving on.",
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

  if (hasBgBash) {
    sections.push(
      [
        `**Long-running commands** (builds, installs, full test suites): run them in the FOREGROUND — use \`${bashName}({ command, wait: true })\` when you know it is long and need the result before anything else; if you send a new message, the wait detaches to background; otherwise omit \`wait\` so auto-promote can hand you a reminder while you work.`,
        "- `background: true` is ONLY for when you have OTHER useful work to do while it runs: start it, do the other work, and the completion reminder delivers the result (or spawn a subagent for the side work). Do NOT background a command and then immediately `bash_watch` it — that spends a whole extra turn waiting for something foreground returns in one.",
        "- `bash_watch` is for blocking on an ALREADY-backgrounded task once you've run out of parallel work (sync — the user can interrupt), or reacting to a specific early output line (async: background:true + pattern). Never loop `bash_status` to wait — it's a one-shot inspector.",
      ].join("\n"),
    );
    sections.push(
      `**PTY / interactive commands**: PTY mode is for interactive REPLs and terminal apps (python, node, bash itself, vim). Start with \`${bashName}({ command: "python", pty: true, background: true })\`, read the screen with \`bash_status({ task_id, output_mode: "screen" })\`, and send input with \`bash_write({ task_id, input: "..." })\`.`,
    );
  }

  // Conditional on the hashline arm being the registered one: a legacy-edit
  // session has no tags and must not be told to go mint them.
  if (opts.hashlineEffective === true) {
    sections.push(HASHLINE_TAG_SOURCE_HINT);
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
  hoistBuiltins: boolean,
  hashlineEffective = false,
): string | null {
  // Background-bash gating reads the resolved bash config so the graduated
  // `bash.background` setting controls whether the hint appears. See
  // `resolveBashConfig` in config.ts.
  return buildWorkflowHints({
    toolSurface: config.tool_surface ?? "recommended",
    hoistBuiltins,
    semanticEnabled: config.semantic_search === true,
    bashBackgroundEnabled: resolveBashConfig(config).background,
    bashCompressionEnabled: resolveBashConfig(config).compress,
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
  hoistBuiltinTools: boolean;
  hoistGrep: boolean;
  hoistBash: boolean;
  hoistEdit: boolean;
  hoistRead: boolean;
}

/**
 * Register the workflow-hints extension on Pi via `before_agent_start`.
 *
 * Pi assembles a fresh system prompt for every turn, then fires
 * `before_agent_start` with the assembled prompt. Our handler appends the
 * AFT workflow hints block to that prompt. If multiple extensions return a
 * `systemPrompt`, Pi chains them — so we always append (never replace).
 */
export function registerWorkflowHints(
  pi: ExtensionAPI,
  config: AftConfig,
  surface: ToolSurfaceFlags,
): void {
  // Build the absent-tools set from the resolved tool surface. In prefixed
  // mode, file/search/bash alternatives are registered under aft_ names while
  // background controls stay unprefixed for AFT task IDs.
  const hoistBuiltins = surface.hoistBuiltinTools;
  const grepName = hoistBuiltins ? "grep" : "aft_grep";
  const bashName = hoistBuiltins ? "bash" : "aft_bash";
  const absent = new Set<string>();
  if (!surface.outline) absent.add("aft_outline");
  if (!surface.zoom) absent.add("aft_zoom");
  if (!surface.semantic) absent.add("aft_search");
  if (!surface.navigate) absent.add("aft_callgraph");
  if (!surface.inspect) absent.add("aft_inspect");
  if (!surface.hoistGrep) absent.add(grepName);
  if (!surface.hoistRead) absent.add(hoistBuiltins ? "read" : "aft_read");
  if (!surface.hoistBash) {
    absent.add(bashName);
    absent.add("bash_status");
  }

  const hintsBlock = buildHintsFromConfig(
    config,
    absent,
    hoistBuiltins,
    piHashlineEffective(config, surface),
  );
  if (!hintsBlock) return;

  log(`Workflow hints injected (${hintsBlock.length} chars)`);

  // Pi's `before_agent_start` handler can return `systemPrompt` to chain
  // an additional system prompt onto the assembled one. We always APPEND
  // — never overwrite — so other extensions' prompt contributions survive.
  (
    pi.on as (
      event: "before_agent_start",
      handler: (event: { systemPrompt: string }) => unknown,
    ) => void
  )("before_agent_start", (event) => {
    return { systemPrompt: `${event.systemPrompt}\n\n${hintsBlock}` };
  });
}
