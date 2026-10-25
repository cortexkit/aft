export const SEARCH_CONTEXT_MAX_CHARACTERS = 6000;

export function capSearchContext(text: string): string {
  return Array.from(text).slice(-SEARCH_CONTEXT_MAX_CHARACTERS).join("");
}
