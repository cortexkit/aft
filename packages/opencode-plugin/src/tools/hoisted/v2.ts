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

/** Bind shared projected definitions to OpenCode's V2 permission endpoint. */
export function hoistedV2ToolConsumers(host: V2PermissionHostContext): V2ToolConsumers {
  return {
    requestPermission: (request, context) => requestPermission(host, request, context),
  };
}
