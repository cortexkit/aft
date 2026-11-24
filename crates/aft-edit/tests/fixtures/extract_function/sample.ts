import { readFile } from "fs";

const BASE_URL = "https://api.example.com";

function processData(items: string[], prefix: string): string {
  const filtered = items.filter(item => item.length > 0);
  const mapped = filtered.map(item => prefix + item);
  const result = mapped.join(", ");
  console.log(result);
  return result;
}

function simpleHelper(x: number): number {
  const doubled = x * 2;
  const added = doubled + 10;
  return added;
}

function voidWork(name: string): void {
  const greeting = "Hello, " + name;
  console.log(greeting);
}
