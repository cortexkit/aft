// Consumer 2 — same directory, imports both formatDate and parseDate
// After move, only formatDate's import should change
import { formatDate, parseDate } from './service';

export function convertDate(input: string): string {
  const date = parseDate(input);
  return formatDate(date);
}
