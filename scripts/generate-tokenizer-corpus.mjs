import fs from "node:fs";
import Tokenizer from "ai-tokenizer";
import * as claudeEncoding from "ai-tokenizer/encoding/claude";

const tokenizer = new Tokenizer(claudeEncoding);

const prose = `Agent File Tools compresses long development sessions by preserving the information that matters most. The tokenizer must be exact because compression savings feed scheduling, budgeting, and bridge-level context decisions across many providers.`;
const code = `export function tokenize(input: string): number {\n  const re = /'s|'t| ?\\p{L}+/gu;\n  return [...input.matchAll(re)].length;\n}\n`;
const json = JSON.stringify(
  {
    model: "claude-3-5-sonnet",
    messages: [
      { role: "system", content: "Count tokens exactly." },
      { role: "user", content: "Hello\\nworld\\t!" },
    ],
    temperature: 0,
  },
  null,
  2,
);
const mixed = "English 中文 日本語 한국어 العربية हिन्दी emoji 🚀✨ and math ∑≈√";
const whitespace = "   \t\t\n\n  leading and trailing whitespace   \n";
const ansi = "\u001b[31mred\u001b[0m \u001b[1mbold\u001b[22m";

const inputs = [
  "",
  "!",
  "a",
  " ",
  "라",
  "中",
  "🚀",
  "hello",
  " hello",
  "decoder",
  " decoder",
  "can't won't they're I've I'm you'll she'd",
  prose,
  prose.repeat(4),
  code,
  "const pattern = /\\s+(?!\\S)| ?[^\\s\\p{L}\\p{N}]+/gu;",
  mixed,
  whitespace,
  "\t\n\r\n",
  "                    ",
  "  surrounded by spaces  ",
  "line one\nline two\n\nline four",
  "literal escapes: \\n \\t \\r \\u001b",
  json,
  ansi,
  "punctuation!!! --- ... ??? ### $$$ %%% &&&",
  "numbers 1234567890 3.14159 -42 +9000 １２３４５",
  "paths /Users/example/project/src/lib.rs and C:\\Temp\\file.txt",
  "SQL: SELECT * FROM users WHERE name = 'Ada' AND active = true;",
  "Markdown **bold** _italic_ `code` [link](https://example.com?q=1&x=2)",
  "JSONL\n" + Array.from({ length: 20 }, (_, i) => JSON.stringify({ i, text: mixed })).join("\n"),
  (prose + "\n" + code + mixed + "\n").repeat(120),
  ("0123456789abcdef ".repeat(1024) + mixed).repeat(1),
];

const corpus = inputs.map((input) => ({ input, expected: tokenizer.count(input) }));

fs.writeFileSync(
  new URL("../crates/aft-tokenizer/tests/snapshot_corpus.json", import.meta.url),
  `${JSON.stringify(corpus, null, 2)}\n`,
);
