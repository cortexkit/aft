// ---------------------------------------------------------------------------
// Workflow hints — short system prompt block teaching the agent
// token-efficient AFT workflows. Mirrors packages/opencode-plugin/src/workflow-hints.ts;
// scheduled to consolidate into a shared package in v0.19 alongside the
// bridge-extraction refactor (see ctx_note #53).
// ---------------------------------------------------------------------------

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import type { AftConfig } from "./config.js";
import { resolveBashConfig } from "./config.js";
import { log } from "./logger.js";

export interface WorkflowHintsOpts {
  toolSurface: "minimal" | "recommended" | "all";
  hoistBuiltins: boolean;
  semanticEnabled: boolean;
  bashBackgroundEnabled: boolean;
  /** Set of tool names KNOWN-ABSENT from the registered surface. */
  absentTools: Set<string>;
}

const HEADING = "## Prefer AFT tools for token efficiency";

export function buildWorkflowHints(opts: WorkflowHintsOpts): string | null {
  const sections: string[] = [];

  // Pi: hoisted built-ins keep their original names (read/grep/bash).
  // Non-hoisted Pi mode is currently not supported — Pi installs hoisted
  // wrappers unconditionally — but we keep the toggle for parity with the
  // OpenCode plugin and v0.19 shared-package extraction.
  const grepName = opts.hoistBuiltins ? "grep" : "aft_grep";
  const bashName = opts.hoistBuiltins ? "bash" : "aft_bash";

  const hasOutline = !opts.absentTools.has("aft_outline");
  const hasZoom = !opts.absentTools.has("aft_zoom");
  const hasGrep = opts.toolSurface !== "minimal" && !opts.absentTools.has(grepName);
  const hasSearch =
    opts.toolSurface !== "minimal" && opts.semanticEnabled && !opts.absentTools.has("aft_search");
  const hasNavigate = opts.toolSurface === "all" && !opts.absentTools.has("aft_navigate");
  const hasBgBash =
    opts.bashBackgroundEnabled &&
    !opts.absentTools.has(bashName) &&
    !opts.absentTools.has("bash_status");

  if (hasOutline && hasZoom) {
    sections.push(
      `**Web/URL access**: \`aft_outline({ target: "<url>" })\` first for structure, then \`aft_zoom({ target: "<url>", symbol: "<heading>" })\` for the specific section.`,
    );
  }

  if (hasOutline && hasZoom && (hasGrep || hasSearch)) {
    const locator = hasGrep ? `\`${grepName}\`` : "`aft_search`";
    sections.push(
      hasGrep && hasSearch
        ? `**Code exploration**: For exact identifiers (\`useState\`, function names, env vars), error messages, or path-shaped queries → \`${grepName}\` first. For broad concepts ('where is X handled', 'how does Y work') → \`aft_search\`. Then use \`aft_outline\` for structure → \`aft_zoom\` for symbol(s).`
        : `**Code exploration**: ${locator} to locate → \`aft_outline\` for structure → \`aft_zoom\` for symbol(s).`,
    );
  }

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

  if (hasBgBash) {
    sections.push(
      `**Long-running commands** (builds, installs, full test suites): \`${bashName}({ background: true })\` returns immediately with a \`task_id\`. A completion reminder is delivered automatically — do not poll \`bash_status({ task_id })\`. Use \`bash_status\` only after the reminder arrives, or to inspect a task you already know is complete.`,
    );
  }

  if (sections.length === 0) {
    return null;
  }

  return `${HEADING}\n\n${sections.join("\n\n")}`;
}

export function buildHintsFromConfig(
  config: AftConfig,
  absentTools: Set<string>,
  hoistBuiltins: boolean,
): string | null {
  // Background-bash gating reads the resolved bash config so the new
  // graduated `bash: true` / `bash: { background: true }` shapes enable
  // the hint, not just the legacy `experimental.bash.background: true`
  // path. See `resolveBashConfig` in config.ts.
  return buildWorkflowHints({
    toolSurface: config.tool_surface ?? "recommended",
    hoistBuiltins,
    semanticEnabled: config.semantic_search === true,
    bashBackgroundEnabled: resolveBashConfig(config).background,
    absentTools,
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
  hoistGrep: boolean;
  hoistBash: boolean;
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
  // Build the absent-tools set from the resolved tool surface. Pi always
  // hoists built-ins (read/grep/bash), so `hoistBuiltins=true`.
  const absent = new Set<string>();
  if (!surface.outline) absent.add("aft_outline");
  if (!surface.zoom) absent.add("aft_zoom");
  if (!surface.semantic) absent.add("aft_search");
  if (!surface.navigate) absent.add("aft_navigate");
  if (!surface.hoistGrep) absent.add("grep");
  if (!surface.hoistBash) {
    absent.add("bash");
    absent.add("bash_status");
  }

  const hintsBlock = buildHintsFromConfig(config, absent, /* hoistBuiltins */ true);
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
