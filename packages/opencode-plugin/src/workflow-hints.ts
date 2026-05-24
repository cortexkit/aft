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
  /** `experimental.bash.background` — gates background-bash paragraph. */
  bashBackgroundEnabled: boolean;
  /** Set of disabled tool names (after surface filtering). */
  disabledTools: Set<string>;
}

const HEADING = "## Prefer AFT tools for token efficiency";

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

  // aft_outline and aft_zoom are present at "minimal" + above. They're never
  // hoisted (always aft-prefixed).
  const hasOutline = !opts.disabledTools.has("aft_outline");
  const hasZoom = !opts.disabledTools.has("aft_zoom");
  const hasGrep = opts.toolSurface !== "minimal" && !opts.disabledTools.has(grepName);
  const hasSearch =
    opts.toolSurface !== "minimal" && opts.semanticEnabled && !opts.disabledTools.has("aft_search");
  // aft_navigate is "all"-tier only.
  const hasNavigate = opts.toolSurface === "all" && !opts.disabledTools.has("aft_navigate");
  const hasBash = !opts.disabledTools.has(bashName);
  const hasBgBash =
    hasBash && opts.bashBackgroundEnabled && !opts.disabledTools.has(bashStatusName);

  // Web/URL access — needs aft_outline + aft_zoom.
  if (hasOutline && hasZoom) {
    sections.push(
      `**Web/URL access**: \`aft_outline({ url })\` first for structure, then \`aft_zoom({ url, symbol: "<heading>" })\` for the specific section.`,
    );
  }

  // Code exploration — needs at least aft_outline + aft_zoom + (grep or aft_search).
  if (hasOutline && hasZoom && (hasGrep || hasSearch)) {
    const locator = hasGrep ? `\`${grepName}\`` : "`aft_search`";
    sections.push(
      hasGrep && hasSearch
        ? `**Code exploration**: For exact identifiers (\`useState\`, function names, env vars), error messages, or path-shaped queries → \`${grepName}\` first. For broad concepts ('where is X handled', 'how does Y work') → \`aft_search\`. Then use \`aft_outline\` for structure → \`aft_zoom\` for symbol(s).`
        : `**Code exploration**: ${locator} to locate → \`aft_outline\` for structure → \`aft_zoom\` for symbol(s).`,
    );
  }

  // Relationship questions — needs aft_navigate ("all" surface).
  if (hasNavigate) {
    sections.push(
      [
        "Use `aft_navigate` instead of grep + read chains for relationship questions:",
        "- `callers` — find all call sites before changing a function signature",
        "- `impact` — blast radius (which functions/files will need updates)",
        "- `trace_to` — how execution reaches this code from entry points (routes, exports, main)",
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
      `**Long-running commands** (builds, installs, full test suites): \`${bashName}({ background: true })\` returns immediately with a \`taskId\`. A completion reminder is delivered automatically — do not poll \`${bashStatusName}({ taskId })\`. Use \`${bashStatusName}\` only after the reminder arrives, or to inspect a task you already know is complete.`,
    );
  }

  if (sections.length === 0) {
    return null;
  }

  return `${HEADING}\n\n${sections.join("\n\n")}`;
}

/**
 * Resolve workflow-hints opts from a loaded AftConfig and the active
 * disabled-tools set computed at registration time.
 *
 * Background-bash gating reads the resolved bash config so the new
 * graduated `bash: true` / `bash: { background: true }` shapes enable the
 * hint, not just the legacy `experimental.bash.background: true` path.
 */
export function buildHintsFromConfig(config: AftConfig, disabledTools: Set<string>): string | null {
  return buildWorkflowHints({
    toolSurface: config.tool_surface ?? "recommended",
    hoistBuiltins: config.hoist_builtin_tools !== false,
    semanticEnabled: config.semantic_search === true,
    bashBackgroundEnabled: resolveBashConfig(config).background,
    disabledTools,
  });
}
