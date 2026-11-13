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

const HEADING = "# Agent Foundational Toolkit";

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

/** Build guidance only for the tools and bash features available in this session. */
export function buildWorkflowHints(opts: WorkflowHintsOpts): string | null {
  const has = (name: string) => !opts.absentTools.has(name);
  const hasBash = has("bash");
  const hasBackground =
    hasBash && opts.bashBackgroundEnabled && has("bash_watch") && has("bash_status");
  const finding: string[] = [];
  if (has("aft_search")) {
    finding.push("start with aft_search; it takes concepts, identifiers, regex and literals.");
  }
  if (has("aft_outline") && has("aft_zoom") && has("read")) {
    finding.push(
      // "Then" only follows the search sentence; alone, the sentence starts at the tool.
      `${finding.length ? "Then " : ""}aft_outline for structure, aft_zoom for named symbols, read for line ranges.`,
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
