// Consumer 1 — same directory, imports only formatDate
import { formatDate } from './service';

export function renderTimestamp(date: Date): string {
  return `[${formatDate(date)}]`;
}
