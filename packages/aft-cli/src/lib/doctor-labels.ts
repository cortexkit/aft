// Pending Ufuk's approval.
export const CLI_BINARY_LABEL =
  "CLI lookup of AFT binary: <version> at <path> (not a probe of the running plugin worker)";
// Pending Ufuk's approval.
export const CLI_VERSION_CACHED_LABEL = "CLI version cached";

export function describeCliBinary(version: string | null, path?: string | null): string {
  return CLI_BINARY_LABEL.replace("<version>", version ?? "unknown").replace(
    "<path>",
    path ?? "unknown",
  );
}
