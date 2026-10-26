import type { AftProjectTransport } from "./transport.js";

export interface ViewToken {
  token: string;
  expires_ms: number;
  advisory: true;
}

/** Mint a provisioning hint. The daemon still verifies every checkout bind. */
export async function mintViewToken(
  transport: AftProjectTransport,
  root: string,
  expectedTree: string,
): Promise<ViewToken> {
  const response = await transport.send("view_token", {
    root,
    expected_tree: expectedTree,
  });
  if (!response.success) {
    throw new Error(typeof response.message === "string" ? response.message : "View token mint failed");
  }
  const data = response;
  if (typeof data !== "object" || data === null) {
    throw new Error("Invalid advisory view token response");
  }
  const value = data as Record<string, unknown>;
  if (
    typeof value.token !== "string" ||
    typeof value.expires_ms !== "number" ||
    value.advisory !== true
  ) {
    throw new Error("Invalid advisory view token response");
  }
  return value as unknown as ViewToken;
}
