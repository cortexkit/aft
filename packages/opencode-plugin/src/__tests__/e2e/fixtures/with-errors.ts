export function messyFormat(value: string): string {
  const result = value.trim();
  if (result.length === 0) {
    return "EMPTY";
  }

  return result.toLowerCase();
}

export const statusMessage = "pending";
export const duplicateMessage = "pending";
