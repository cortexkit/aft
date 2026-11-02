// Approved by Ufuk (2026-11-02).
export const CLI_BINARY_LABEL =
  "AFT binary this CLI found: <version> at <path> (the plugin may run a different one)";
// Approved by Ufuk (2026-11-02).
export const CLI_VERSION_CACHED_LABEL = "Cached binary for this CLI";

export function describeCliBinary(version: string | null, path?: string | null): string {
  return CLI_BINARY_LABEL.replace("<version>", version ?? "unknown").replace(
    "<path>",
    path ?? "unknown",
  );
}
