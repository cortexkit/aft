import { readFileSync } from "node:fs";
import { join } from "node:path";

export const DEFAULT_SUFFIX = "!";
const LOCAL_SEPARATOR = ":";

function normalize(input: string): string {
  return input.trim().toUpperCase();
}

function decorate(input: string): string {
  return `[${input}]`;
}

export function funcA(input: string): string {
  return `A${LOCAL_SEPARATOR}${input}`;
}

export function funcB(name: string): string {
  return `${normalize(name)}${DEFAULT_SUFFIX}`;
}

export function funcC(parts: string[]): string {
  return decorate(parts.join(join("", "-")));
}

export class SampleService {
  greet(name: string): string {
    return funcB(name);
  }

  readFirstLine(filePath: string): string {
    return readFileSync(filePath, "utf8").split("\n")[0] ?? "";
  }
}
