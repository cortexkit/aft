// ---------------------------------------------------------------------------
// Workflow hints — short system prompt block teaching the agent
// token-efficient AFT workflows.
//
// Conditional on the registered tools so we never advertise tools the agent
// doesn't have. A tool is registered unless it is in `disabled_tools`.
// ---------------------------------------------------------------------------

import { type AftConfig, resolveBashConfig } from "./config.js";
import { hashlineTagSourceSentence } from "./hashline-tag-sources.js";

export interface WorkflowHintsOpts {
  /** `bash.background` — gates background-bash paragraph. */
  bashBackgroundEnabled: boolean;
  /** Resolved bash compression flag. */
  bashCompressionEnabled: boolean;
  /**
   * Resolved `bash.rewrite` flag. Only rewritten `cat`/`head`/`tail` commands
   * mint hashline tags through bash, so the hashline hint names them only when
   * rewriting is on. Omitted means on.
   */
  bashRewriteEnabled?: boolean;
  /** Set of tool names that are not registered. */
  disabledTools: Set<string>;
  /** Whether the hashline `edit` arm is the one actually registered. */
  hashlineEffective?: boolean;
}

const HEADING = "# Agent Foundational Toolkit";

/**
 * Routing rule for hashline sessions: which calls mint the tags a patch
 * addresses (see `hashlineTagSourceSentence`). The bash rewrite path and each
 * AFT navigation tool are named only when present.
 */
export function hashlineTagSourceHint(
  bashRewritesMintTags: boolean,
  isRegistered: (toolName: string) => boolean = () => true,
): string {
  return `**Hashline edit tags**: ${hashlineTagSourceSentence(bashRewritesMintTags, isRegistered)}`;
}

/** The hashline hint for a session whose bash rewrites `cat`/`head`/`tail`. */
export const HASHLINE_TAG_SOURCE_HINT = hashlineTagSourceHint(true);

/** Build guidance only for the tools and bash features available in this session. */
export function buildWorkflowHints(opts: WorkflowHintsOpts): string | null {
  const has = (name: string) => !opts.disabledTools.has(name);
  const hasBash = has("bash");
  const hasBackground =
    hasBash && opts.bashBackgroundEnabled && has("bash_watch") && has("bash_status");
  const finding: string[] = [];
  if (has("aft_search")) {
    finding.push("start with aft_search; it takes concepts, identifiers, regex and literals.");
  }
  if (has("aft_outline") && has("aft_zoom") && has("read")) {
    finding.push(
      "Then aft_outline for structure, aft_zoom for named symbols, read for line ranges.",
    );
  }
  if (hasBash) {
    finding.push("Don't find or read code with grep, find, sed or cat in bash.");
  }

  const commands: string[] = [];
  if (hasBash && opts.bashCompressionEnabled) {
    commands.push(
      "don't pipe test or build output into tail, grep or head; AFT already shortens it.",
    );
  }
  if (hasBackground) {
    commands.push(
      "Pass wait: true when you need a long command's result before anything else; a new message from the user detaches the wait.",
      "Otherwise a long command moves to the background and a reminder brings its result.",
      "Block on it with bash_watch only when nothing else is left; bash_watch with background: true and a pattern tells you when a line appears.",
      "Never loop bash_status.",
    );
  }

  const sections = [
    finding.length ? `**Finding code**: ${finding.join(" ")}` : "",
    has("aft_callgraph")
      ? "**Relationships**: aft_callgraph for callers, impact, and execution or data-flow traces."
      : "",
    has("aft_outline") && has("aft_zoom")
      ? "**Web pages**: aft_outline with a URL as target, then aft_zoom on a heading."
      : "",
    // Reserve the parallel-calls position until another section makes the block useful.
    "",
    has("aft_inspect")
      ? "**After edits**: run aft_inspect for diagnostics; the project's typecheck and tests are the final check."
      : "",
    has("aft_inspect")
      ? "**Status bar**: `[AFT E.. W.. | D.. U.. C.. | T..]` appears when a count changes. E above 0 usually means your last edit broke something; `?` means unknown, not zero."
      : "",
    commands.length ? `**Commands**: ${commands.join(" ")}` : "",
  ];
  const hashline =
    opts.hashlineEffective === true
      ? hashlineTagSourceHint(hasBash && opts.bashRewriteEnabled !== false, has)
      : "";
  if (!sections.some(Boolean) && !hashline) return null;

  sections[3] = "**Parallel calls**: send independent read-only calls in one response.";
  const text = `${HEADING}\n\n${sections.filter(Boolean).join("\n")}`;
  return hashline ? `${text}\n\n${hashline}` : text;
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
  const bashCfg = resolveBashConfig(config);
  return buildWorkflowHints({
    bashBackgroundEnabled: bashCfg.background,
    bashCompressionEnabled: bashCfg.compress,
    bashRewriteEnabled: bashCfg.rewrite,
    disabledTools,
    hashlineEffective,
  });
}

/** Every tool name a workflow-hints section can mention. */
export const WORKFLOW_HINT_TOOL_NAMES: readonly string[] = [
  "aft_outline",
  "aft_zoom",
  "aft_search",
  "aft_callgraph",
  "aft_inspect",
  "grep",
  "read",
  "bash",
  "bash_status",
  "bash_watch",
  "bash_write",
];

/**
 * Build the hints block for the tools that are actually registered. Every
 * hint-relevant name missing from `registeredTools` is passed as absent, so
 * sections for tools the agent cannot call are dropped.
 */
export function buildHintsForRegisteredTools(
  config: AftConfig,
  registeredTools: ReadonlySet<string>,
  hashlineEffective = false,
): string | null {
  const absent = new Set(WORKFLOW_HINT_TOOL_NAMES.filter((name) => !registeredTools.has(name)));
  return buildHintsFromConfig(config, absent, hashlineEffective);
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
