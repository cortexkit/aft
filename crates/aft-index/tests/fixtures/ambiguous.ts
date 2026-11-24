// Fixture with duplicate symbol names for disambiguation testing

export function process(data: string): string {
  return data.toLowerCase();
}

export class DataHandler {
  process(items: string[]): string[] {
    return items.map(i => i.trim());
  }
}
