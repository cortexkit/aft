// Consumer 3 — same directory, uses aliased import
import { formatDate as fmtDate } from './service';

export function logDate(date: Date): void {
  console.log(`Date: ${fmtDate(date)}`);
}
