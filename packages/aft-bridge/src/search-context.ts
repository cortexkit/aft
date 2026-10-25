export function capSearchContext(text: string): string {
  return Array.from(text).slice(-6000).join("");
}
