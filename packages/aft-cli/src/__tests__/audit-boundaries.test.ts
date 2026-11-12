import { expect, test } from "bun:test";
import { capBodyToGithubLimit } from "../lib/issue-body.js";
import { redactUsername, sanitizeContent } from "../lib/sanitize.js";

test("issue truncation preserves both log fences", () => {
  const body = `## Logs (last lines)\n\n\`\`\`text\n${"old line\n".repeat(1000)}newest\n\`\`\`\n\n## Recent errors\nerror`;
  const result = capBodyToGithubLimit(body, 500);
  expect(result.match(/^```.*$/gm)).toHaveLength(2);
  expect(result).toContain("truncated");
  expect(result).toContain("newest");
  expect(Buffer.byteLength(result)).toBeLessThanOrEqual(500);
});

test("distinctive username redaction preserves word boundaries but masks path components", () => {
  const username = "alicejones";
  expect(
    sanitizeContent(
      `project_${username} ${username}@host /${username}/ \\${username}\\ ~${username}`,
      { username },
    ),
  ).toBe(`project_${username} <USER>@host /<USER>/ \\<USER>\\ ~<USER>`);
});

test("common usernames redact only in identity positions", () => {
  for (const user of ["root", "dev"]) {
    expect(
      redactUsername(`project_${user} ${user}2 /${user}/ \\${user}\\ ~${user} ${user}@host`, user),
    ).toBe(`project_${user} ${user}2 /${user}/ \\${user}\\ ~<USER> <USER>@host`);
  }
});

test("short distinctive usernames still redact whole words including system paths", () => {
  expect(redactUsername("project_a a2 /a/ \\a\\ ~a a@host /usr/a", "a")).toBe(
    "project_a a2 /<USER>/ \\<USER>\\ ~<USER> <USER>@host /usr/<USER>",
  );
});

test("issue truncation preserves multiple harness fences and a huge line", () => {
  const body = `## Logs (last lines)\n#### first\n\`\`\`text\n${"x".repeat(4000)}\n\`\`\`\n#### second\n\`\`\`text\n${"y".repeat(4000)}\n\`\`\`\n## Recent errors\nerror`;
  const result = capBodyToGithubLimit(body, 500);
  expect(result.match(/^```.*$/gm)).toHaveLength(4);
  expect(Buffer.byteLength(result)).toBeLessThanOrEqual(500);
});
