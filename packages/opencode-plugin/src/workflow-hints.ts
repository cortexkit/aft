// ---------------------------------------------------------------------------
// Workflow hints — short system prompt block teaching the agent
// token-efficient AFT workflows.
//
// Conditional on the actual tool surface so we never advertise tools the
// agent doesn't have. Tool name resolution honors `hoist_builtin_tools`:
// when hoisting is on (default) the agent sees `read`/`grep`/`bash`; when
// off it sees `aft_read`/`aft_grep`/`aft_bash`.
// ---------------------------------------------------------------------------

import { type AftConfig, resolveBashConfig } from "./config.js";

export interface WorkflowHintsOpts {
  /** `tool_surface` setting — controls which tools are registered. */
  toolSurface: "minimal" | "recommended" | "all";
  /** `hoist_builtin_tools` setting — affects tool name (read vs aft_read). */
  hoistBuiltins: boolean;
  /** `experimental.semantic_search` — gates `aft_search` mention. */
  semanticEnabled: boolean;
  /** `bash.background` — gates background-bash paragraph. */
  bashBackgroundEnabled: boolean;
  /** Resolved bash compression flag. */
  bashCompressionEnabled: boolean;
  /** Set of disabled tool names (after surface filtering). */
  disabledTools: Set<string>;
  /** Whether the hashline `edit` arm is the one actually registered. */
  hashlineEffective?: boolean;
}

const HEADING = "## IMPORTANT NOTICE about your tools";

/**
 * Routing rule for hashline sessions.
 *
 * Navigation tools return source that looks edit-ready but never publishes a
 * snapshot, so an agent that inspects a symbol and then patches it is refused
 * for a tag it believes it already has. Naming the tag-minting tools is the
 * only way to distinguish "I have seen this code" from "I can address it".
 */
export const HASHLINE_TAG_SOURCE_HINT =
  "**Hashline edit tags**: Only `read` (and accepted AFT `cat`/`head`/`tail` rewrites) mint hashline tags. `aft_zoom`, `aft_outline`, `grep`, `aft_search`, and conflict snippets do not. After navigation, call `read` on every file and range the patch addresses.";

/**
 * Build the workflow hints block. Returns `null` when no hints are
 * applicable for the configured surface (e.g. `tool_surface: "minimal"`
 * with no aft_outline/aft_zoom available — only safety tool is registered).
 */
export function buildWorkflowHints(opts: WorkflowHintsOpts): string | null {
  const sections: string[] = [];

  // Tool name resolution. When hoisting is on, OpenCode sees built-in
  // names; when off, agent-visible names are aft-prefixed.
  const grepName = opts.hoistBuiltins ? "grep" : "aft_grep";
  const bashName = opts.hoistBuiltins ? "bash" : "aft_bash";
  const bashStatusName = "bash_status";
  const bashWriteName = "bash_write";

  // aft_outline and aft_zoom are present at "minimal" + above. They're never
  // hoisted (always aft-prefixed).
  const hasOutline = !opts.disabledTools.has("aft_outline");
  const hasZoom = !opts.disabledTools.has("aft_zoom");
  const readName = opts.hoistBuiltins ? "read" : "aft_read";
  const hasRead = !opts.disabledTools.has(readName);
  const hasGrep = opts.toolSurface !== "minimal" && !opts.disabledTools.has(grepName);
  const hasSearch =
    opts.toolSurface !== "minimal" && opts.semanticEnabled && !opts.disabledTools.has("aft_search");
  // aft_callgraph is "all"-tier only.
  const hasNavigate = opts.toolSurface === "all" && !opts.disabledTools.has("aft_callgraph");
  const hasInspect = opts.toolSurface !== "minimal" && !opts.disabledTools.has("aft_inspect");
  const hasBash = !opts.disabledTools.has(bashName);
  const hasBgBash =
    hasBash && opts.bashBackgroundEnabled && !opts.disabledTools.has(bashStatusName);

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

  // Web/URL access — needs aft_outline + aft_zoom.
  if (hasOutline && hasZoom) {
    sections.push(
      `**Web/URL access**: \`aft_outline({ target: url })\` first for structure, then \`aft_zoom({ url, symbols: "<heading>" })\` for the specific section.`,
    );
  }

  // Code exploration — needs at least aft_outline + aft_zoom + (grep or aft_search).
  // Lead with the two behaviors agents reliably get wrong: serializing
  // independent lookups, and shelling out to grep for code search. Both are
  // stated imperatively (DO NOT) because soft "prefer" wording does not change
  // the reflex. When aft_search is available it is named alone — it auto-routes
  // literals too, so naming the grep tool would only dilute the redirect; only
  // when aft_search is absent do we point at the grep TOOL as the indexed,
  // ranked alternative to raw bash grep.
  if (hasOutline && (hasGrep || hasSearch) && (hasZoom || hasRead)) {
    const searchName = hasSearch ? "aft_search" : grepName;
    const locate = hasSearch
      ? "`aft_search` is the primary code-search tool: one call auto-routes concepts, identifiers, regex, error strings, and literals."
      : `\`${grepName}\` (the tool — indexed and ranked) locates code.`;
    const zoomSteer = hasZoom ? ", or aft_zoom" : "";
    const searchSteer = hasSearch
      ? `use aft_search (concepts, identifiers, regex, literals), ${readName}, aft_outline${zoomSteer} instead`
      : `use the ${grepName} tool, ${readName}, aft_outline${zoomSteer} instead`;
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

  // Codebase health & diagnostics — needs aft_inspect (recommended+).
  // Lead with the behavioral change: AFT no longer auto-surfaces compile/type
  // errors on edit, so the agent MUST pull them. Anchor to the edit→test/commit
  // moment, and be explicit that aft_inspect diagnostics are a checkpoint, not
  // the authority (the project checker is).
  if (hasInspect) {
    sections.push(
      "**Codebase health & diagnostics**: AFT does not surface compile/type errors automatically after edits — pull them with `aft_inspect`. Run it after a batch of edits and before you run tests or commit, when starting in unfamiliar code, or before a refactor/review. One call summarizes diagnostics (compile/type errors), TODOs, metrics, dead code, unused exports, and duplicates; pass `sections` for focused drill-down and `scope` to actively pull diagnostics for a specific file or directory. Its diagnostics are a fast checkpoint, not the authority — a clean `tsc` / `cargo check` / `pyright` run is the real gate. Treat stale_categories/pending_categories as stale or incomplete cache state. AFT schedules a Tier-2 refresh after its next idle or inspect-triggered background run; use one later normal aft_inspect after that refresh, not a polling loop.",
    );
    // Status-bar legend — taught once here so the per-call bar is just compact
    // values (~18 tokens). The bar is appended to tool results on change.
    sections.push(
      "**AFT status bar**: tool results may end with a one-line health bar `[AFT E<errors> W<warnings> | D<dead-code> U<unused-exports> C<clone/dup-groups> | T<todos>]` — an IDE-style glance that appears when a count changes. `E`/`W` are live LSP diagnostics for files touched this session (your universal compile-error signal across every language with an LSP). A `~` before `D` means the dead-code/unused/dup counts predate your latest edit — run `aft_inspect` for current numbers and detail. When `E>0`, you likely just introduced errors; investigate before moving on.",
    );
  }

  // Relationship questions — needs aft_callgraph ("all" surface).
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

  // Bash long-running guidance — only add the background-pattern hint when
  // background bash is enabled. Foreground bash now auto-promotes after a
  // short wait-window, so agents never need to know about timeouts up front;
  // there's no "30s default" to warn about anymore.
  if (hasBash && hasBgBash) {
    sections.push(
      [
        `**Long-running commands** (builds, installs, full test suites): run them in the FOREGROUND — use \`${bashName}({ command, wait: true })\` when you know it is long and need the result before anything else; if you send a new message, the wait detaches to background; otherwise omit \`wait\` so auto-promote can hand you a reminder while you work.`,
        "- `background: true` is ONLY for when you have OTHER useful work to do while it runs: start it, do the other work, and the completion reminder delivers the result (or spawn a subagent for the side work). Do NOT background a command and then immediately `bash_watch` it — that spends a whole extra turn waiting for something foreground returns in one.",
        "- `bash_watch` is for blocking on an ALREADY-backgrounded task once you've run out of parallel work (sync — the user can interrupt), or reacting to a specific early output line (async: background:true + pattern). Never loop `bash_status` to wait — it's a one-shot inspector.",
      ].join("\n"),
    );
    sections.push(
      `**PTY / interactive commands**: PTY mode is for interactive REPLs and terminal apps (python, node, bash itself, vim). Start with \`${bashName}({ command: "python", pty: true, background: true })\`, read the screen with \`${bashStatusName}({ taskId, outputMode: "screen" })\`, and send input with \`${bashWriteName}({ taskId, input: "..." })\`.`,
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

  // The opening notice frames the whole block: these are not ordinary
  // CLI-equivalent tools, and the single biggest efficiency win is firing
  // independent read-only calls together. Prepended so it leads, and only
  // when there's real content below it (never emitted alone).
  sections.unshift(
    "You are equipped with a non-standard tool set: indexed code search, symbol-level reading, structural editing, and code analysis that are faster, more precise, and far cheaper in tokens than stitching together command-line utilities in bash. Always reach for these tools first.\n\n**Parallel tool calls**: when several read-only operations are independent, emit them in ONE response instead of serializing — file reads, structure and symbol lookups, code search, diagnostics, and git status/diff/log. Sequence only when a call depends on a prior result or when a command mutates state.",
  );

  return `${HEADING}\n\n${sections.join("\n\n")}`;
}

/**
 * Resolve workflow-hints opts from a loaded AftConfig and the active
 * disabled-tools set computed at registration time.
 *
 * Background-bash gating reads the resolved bash config so the graduated
 * `bash.background` setting controls whether the hint appears.
 */
export function buildHintsFromConfig(
  config: AftConfig,
  disabledTools: Set<string>,
  hashlineEffective = false,
): string | null {
  return buildWorkflowHints({
    toolSurface: config.tool_surface ?? "recommended",
    hoistBuiltins: config.hoist_builtin_tools !== false,
    semanticEnabled: config.semantic_search === true,
    bashBackgroundEnabled: resolveBashConfig(config).background,
    bashCompressionEnabled: resolveBashConfig(config).compress,
    disabledTools,
    hashlineEffective,
  });
}

/**
 * Fold the hints block into a host `system[]` array in place.
 *
 * Each `system[]` entry becomes its own `role:"system"` message on the wire,
 * and Qwen-family chat templates (vLLM/SGLang, and LiteLLM fronting them)
 * reject any system message past index 0 — so the block must extend the
 * existing entry rather than arrive as a second message. Exported separately
 * from the plugin wiring so the single-message property is unit-testable.
 */
export function appendHintsToSystem(system: string[], hintsBlock: string): void {
  if (!hintsBlock) return;
  if (system.length > 0) {
    const last = system.length - 1;
    system[last] = `${system[last]}\n\n${hintsBlock}`;
  } else {
    system.push(hintsBlock);
  }
}
