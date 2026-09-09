export function leaf(value: string): string { return value; }
export function middle(value: string): string { return leaf(value); }
export function root(value: string): string { return middle(value); }
export function uniqueScenarioSymbol(): string { return root('fixture'); }
