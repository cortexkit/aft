// Consumer 4 — same directory, imports DATE_FORMAT only (should NOT be modified)
import { DATE_FORMAT } from './service';

export function getFormat(): string {
  return DATE_FORMAT;
}
