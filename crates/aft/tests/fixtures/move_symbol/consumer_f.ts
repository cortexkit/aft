// Consumer 6 — same directory, imports parseDate only (should NOT be modified)
import { parseDate } from './service';

export function fromString(s: string): Date {
  return parseDate(s);
}
