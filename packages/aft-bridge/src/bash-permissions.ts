import { log } from "./active-logger.js";

export interface BashPermissionAsk {
  kind?: string;
  patterns?: string[];
  always?: string[];
  command?: string;
  cwd?: string;
  grant_id?: string;
  operation?: string;
  reason?: string;
}

export const BASH_PERMISSION_NOT_SETTLED =
  "bash permission not settled after {rounds} rounds: still asking for {kind} {path} (operation: {operation}; granted already: {granted})";

// Keep the shared refusal wording identical across host integrations.
export const BASH_PERMISSION_DENIED =
  "Permission denied for this shell command: one or more of these parts was denied: {parts} ({host reason}). The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.";

export const BASH_PERMISSION_DENIED_KNOWN =
  "Permission denied for this shell command: the host denied {parts} ({host reason}). These parts were not denied: {not denied parts}. The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.";

export class BashPermissionLoopError extends Error {}

export function bashPermissionRetryGrants(ask: BashPermissionAsk): string[] {
  if (ask.kind === "escalation") return ask.grant_id ? [ask.grant_id] : [];
  return ask.always?.length ? ask.always : (ask.patterns ?? []);
}

export function groupBashPermissionAsks(asks: readonly BashPermissionAsk[]): BashPermissionAsk[] {
  const grouped: BashPermissionAsk[] = [];
  let bash: BashPermissionAsk | undefined;
  for (const ask of asks) {
    if (ask.kind !== "bash" || ask.reason) {
      grouped.push(ask);
      continue;
    }
    if (!bash) {
      bash = { kind: "bash", patterns: [], always: [] };
      grouped.push(bash);
    }
    bash.patterns = [...new Set([...(bash.patterns ?? []), ...(ask.patterns ?? [])])];
    bash.always = [...new Set([...(bash.always ?? []), ...(ask.always ?? [])])];
  }
  return grouped;
}

export function escalationAskText(ask: BashPermissionAsk): string {
  return `This command will run UNSANDBOXED on the host.\n\nExact command:\n${ask.command ?? ""}\n\nWorking directory:\n${ask.cwd ?? ""}`;
}

function permissionAskParts(ask: BashPermissionAsk): string[] {
  const paths = ask.patterns?.length ? ask.patterns : bashPermissionRetryGrants(ask);
  return ask.kind === "escalation" && ask.command
    ? [`${ask.command}${ask.cwd ? ` @ ${ask.cwd}` : ""}`]
    : paths.length
      ? paths
      : ["<no asks returned>"];
}

function permissionAskPath(ask: BashPermissionAsk): string {
  return permissionAskParts(ask).join(", ");
}

function hostWildcardMatches(input: string, pattern: string): boolean {
  // OpenCode @opencode/core 2.1.0, dist/util/wildcard.js:1-7 re-exports this
  // matcher from dist/chunks/shell-y1fvbwt7.js:11-16 (src/util/wildcard.ts).
  const normalized = input.replaceAll("\\", "/");
  let escaped = pattern
    .replaceAll("\\", "/")
    .replace(/[.+^${}()|[\]\\]/g, "\\$&")
    .replace(/\*/g, ".*")
    .replace(/\?/g, ".");
  if (escaped.endsWith(" .*")) escaped = `${escaped.slice(0, -3)}( .*)?`;
  return new RegExp(`^${escaped}$`, process.platform === "win32" ? "si" : "s").test(normalized);
}

interface HostPermissionRule {
  action: string;
  resource: string;
  effect: "allow" | "deny" | "ask";
}

function isHostPermissionRule(rule: unknown): rule is HostPermissionRule {
  return (
    typeof rule === "object" &&
    rule !== null &&
    "action" in rule &&
    typeof rule.action === "string" &&
    "resource" in rule &&
    typeof rule.resource === "string" &&
    "effect" in rule &&
    (rule.effect === "allow" || rule.effect === "deny" || rule.effect === "ask")
  );
}

function hostDeniedParts(error: unknown, parts: readonly string[]): Set<string> {
  const denied = new Set<string>();
  if (typeof error !== "object" || error === null || !("_tag" in error)) return denied;
  const blocked = error._tag === "Tool.Error" && "error" in error ? error.error : error;
  if (
    typeof blocked !== "object" ||
    blocked === null ||
    !("_tag" in blocked) ||
    blocked._tag !== "Permission.BlockedError" ||
    !("permission" in blocked) ||
    blocked.permission !== "shell" ||
    !("resources" in blocked) ||
    !Array.isArray(blocked.resources) ||
    !("rules" in blocked) ||
    !Array.isArray(blocked.rules) ||
    !blocked.rules.every(isHostPermissionRule)
  ) {
    return denied;
  }
  for (const part of parts) {
    if (!blocked.resources.includes(part)) continue;
    // OpenCode's last matching rule wins: @opencode/core 2.1.0,
    // dist/chunks/shell-envzrdm3.js:117-122 (src/permission.ts).
    for (let index = blocked.rules.length - 1; index >= 0; index--) {
      const rule = blocked.rules[index];
      if (
        !rule ||
        !hostWildcardMatches(blocked.permission, rule.action) ||
        !hostWildcardMatches(part, rule.resource)
      ) {
        continue;
      }
      if (rule.effect === "deny") denied.add(part);
      break;
    }
  }
  return denied;
}

/** Hosts provide approval UI; this helper passes approved grants to retries and limits rounds. */
export async function withBashPermissionLoop(options: {
  request: (granted: readonly string[]) => Promise<Record<string, unknown>>;
  approve: (asks: readonly BashPermissionAsk[]) => Promise<void>;
  operation?: string;
}): Promise<Record<string, unknown>> {
  const granted = new Set<string>();
  const alreadyGranted = (ask: BashPermissionAsk) => {
    const grants = bashPermissionRetryGrants(ask);
    return grants.length > 0 && grants.every((grant) => granted.has(grant));
  };
  for (let round = 0; ; round++) {
    const response = await options.request([...granted]);
    if (response.success !== false || response.code !== "permission_required") return response;
    const asks = Array.isArray(response.asks) ? (response.asks as BashPermissionAsk[]) : [];
    log("bash permission round", {
      round,
      operation: options.operation ?? "bash",
      asks: asks.map((ask) => ({
        kind: ask.kind,
        patterns: ask.patterns,
        always: ask.always,
        already_granted: alreadyGranted(ask),
      })),
    });
    const unsettled = () => {
      const message = (asks.length ? asks : [{}])
        .map((ask) => {
          return BASH_PERMISSION_NOT_SETTLED.replace("{rounds}", String(round))
            .replace("{kind}", ask.kind ?? "unknown")
            .replace("{operation}", ask.operation ?? options.operation ?? "bash")
            .replace("{granted}", alreadyGranted(ask) ? "yes" : "no")
            .replace("{path}", () => permissionAskPath(ask));
        })
        .join("; ");
      return new BashPermissionLoopError(message);
    };
    if (asks.length === 0 || asks.every(alreadyGranted) || round >= 8) throw unsettled();
    const pendingAsks = asks.filter((ask) => !alreadyGranted(ask));
    try {
      await options.approve(pendingAsks);
    } catch (error) {
      const parts = pendingAsks.flatMap(permissionAskParts);
      const denied = hostDeniedParts(error, parts);
      const quotedParts = (parts: readonly string[]) =>
        parts.map((part) => `\`${part}\``).join(", ");
      const notDenied = parts.filter((part) => !denied.has(part));
      let message = denied.size ? BASH_PERMISSION_DENIED_KNOWN : BASH_PERMISSION_DENIED;
      if (denied.size && notDenied.length === 0) {
        message = message.replace(" These parts were not denied: {not denied parts}.", "");
      }
      // The host's reason sits in parentheses, so drop its own final period.
      // Without a reason, drop the parenthetical rather than inventing one.
      const reason =
        error instanceof Error && error.message ? error.message.trim().replace(/\.$/, "") : "";
      if (!reason) message = message.replace(" ({host reason})", "");
      const replacements: Record<string, string> = {
        "{parts}": quotedParts(denied.size ? [...denied] : parts),
        "{not denied parts}": quotedParts(notDenied),
        "{host reason}": reason,
      };
      message = message.replace(
        /\{parts\}|\{not denied parts\}|\{host reason\}/g,
        (placeholder) => replacements[placeholder] ?? placeholder,
      );
      throw new BashPermissionLoopError(message, { cause: error });
    }
    for (const ask of asks) {
      for (const grant of bashPermissionRetryGrants(ask)) granted.add(grant);
    }
  }
}
