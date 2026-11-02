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

// Approved by Ufuk: explain that a host refusal applies to the whole shell command.
export const BASH_PERMISSION_DENIED =
  "Permission denied for this shell command: {parts} ({host reason}). The parts are checked together, so one denied part denies the whole command; run the allowed parts in a separate call.";

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

function permissionAskPath(ask: BashPermissionAsk): string {
  const paths = ask.patterns?.length ? ask.patterns : bashPermissionRetryGrants(ask);
  return ask.kind === "escalation" && ask.command
    ? `${ask.command}${ask.cwd ? ` @ ${ask.cwd}` : ""}`
    : paths.length
      ? paths.join(", ")
      : "<no asks returned>";
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
      const parts = pendingAsks.map(permissionAskPath).join(", ");
      // The host's reason sits in parentheses, so drop its own final period.
      // Without a reason, drop the parenthetical rather than inventing one.
      const reason =
        error instanceof Error && error.message ? error.message.trim().replace(/\.$/, "") : "";
      const message = reason
        ? BASH_PERMISSION_DENIED.replace("{host reason}", () => reason)
        : BASH_PERMISSION_DENIED.replace(" ({host reason})", "");
      throw new BashPermissionLoopError(
        message.replace("{parts}", () => parts),
        { cause: error },
      );
    }
    for (const ask of asks) {
      for (const grant of bashPermissionRetryGrants(ask)) granted.add(grant);
    }
  }
}
