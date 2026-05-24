// Source file — contains symbols to be moved
// formatDate will be moved to utils.ts

export function formatDate(date: Date): string {
  const year = date.getFullYear();
  const month = String(date.getMonth() + 1).padStart(2, '0');
  const day = String(date.getDate()).padStart(2, '0');
  return `${year}-${month}-${day}`;
}

export function parseDate(dateStr: string): Date {
  const [year, month, day] = dateStr.split('-').map(Number);
  return new Date(year, month - 1, day);
}

export const DATE_FORMAT = 'YYYY-MM-DD';

export class DateHelper {
  format(date: Date): string {
    return formatDate(date);
  }
}
