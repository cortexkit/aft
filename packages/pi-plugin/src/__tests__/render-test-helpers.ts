/// <reference path="../bun-test.d.ts" />

import type { AgentToolResult, Theme } from "@earendil-works/pi-coding-agent";
import type { Component } from "@earendil-works/pi-tui";
import type { RenderContextLike } from "../tools/render-helpers.js";

export const mockTheme = {
  fg: (_color: string, text: string) => text,
  bold: (text: string) => text,
  inverse: (text: string) => text,
} as unknown as Theme;

export function makeResult(text = "", details?: unknown): AgentToolResult<unknown> {
  return {
    content: [{ type: "text", text }],
    details,
  };
}

export function makeContext<TArgs>(
  args: TArgs,
  overrides: Partial<RenderContextLike<TArgs>> = {},
): RenderContextLike<TArgs> {
  return {
    args,
    lastComponent: undefined,
    isError: false,
    ...overrides,
  };
}

export function renderToString(component: Component): string {
  return stripAnsi(component.render(200).join("\n")).trim();
}

export function stripAnsi(text: string): string {
  let output = "";
  for (let index = 0; index < text.length; index += 1) {
    const char = text[index];
    if (char === "\u001B" && text[index + 1] === "[") {
      index += 2;
      while (index < text.length && !/[A-Za-z]/.test(text[index])) {
        index += 1;
      }
      continue;
    }
    output += char;
  }
  return output;
}
