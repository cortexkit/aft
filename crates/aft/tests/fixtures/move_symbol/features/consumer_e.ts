// Consumer 5 — subdirectory, imports formatDate with ../ path
import { formatDate } from '../service';

export function featureDate(date: Date): string {
  return `Feature: ${formatDate(date)}`;
}
