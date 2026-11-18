export const IGNORED_ENTRIES_SURFACE_ID = "grep..payload.ignored_entries";
export const IGNORED_ENTRIES_DISCLOSURE_PREFIX = "Not searched (ignore rules):";

// This list is an informational preview, not another match-page trailer. Pin
// its two fixture sentences independently of the product's text formatter.
export function expectedIgnoredEntriesDisclosure(fixture: unknown): string | undefined {
  if (fixture === "complete") {
    return "Not searched (ignore rules): ignored-a/ in this folder. Name a path inside one to search it.";
  }
  if (fixture === "incomplete") {
    return "Not searched (ignore rules): ignored-a/, ignored-b/, ignored-c/ and 3 more in this folder. Name a path inside one to search it.";
  }
  return undefined;
}
