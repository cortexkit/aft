import { requestPermission, type V2PermissionHostContext } from "../../permissions/v2.js";
import type { V2ToolConsumers } from "../definitions/v2.js";

export const V2_BUILTIN_REPLACEMENTS = ["read", "edit", "write", "apply_patch"] as const;
export const V2_AFT_FILESYSTEM_TOOLS = ["aft_delete", "aft_move"] as const;

export const V2_PERMISSION_ASK_INVENTORY = [
  "read",
  "edit",
  "write",
  "apply_patch",
  "aft_delete",
  "aft_move",
  "bash:withPermissionLoop",
  "bash:host-fallback",
] as const;

function domainMethod(host: object, domain: string, method: string): boolean {
  const value = (host as Record<string, unknown>)[domain];
  if (!value || typeof value !== "object") return false;
  return typeof (value as Record<string, unknown>)[method] === "function";
}

/**
 * Bind shared projected definitions to OpenCode 2's permission rules.
 *
 * A V2 plugin context has no endpoint for opening a permission prompt, so the
 * evaluator reads the host's configured rules instead. The probe therefore
 * looks for the two domains that carry those rules, and a context missing
 * either one is left without an evaluator so every ask-site refuses rather
 * than acting unchecked.
 */
export function hoistedV2ToolConsumers(host: V2PermissionHostContext | object): V2ToolConsumers {
  if (!domainMethod(host, "agent", "get") || !domainMethod(host, "session", "get")) return {};
  return {
    requestPermission: (request, context) =>
      requestPermission(host as V2PermissionHostContext, request, context),
  };
}
