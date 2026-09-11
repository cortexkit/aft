import { dataMiddle } from "./data-middle";

function leaf(value: string): string { return value; }
function middle(value: string): string { return leaf(value); }
function root(value: string): string { return middle(value); }
export function uniqueScenarioSymbol(): string { return root("fixture"); }

export function dataRoot(value: string): string { return dataMiddle(value); }
