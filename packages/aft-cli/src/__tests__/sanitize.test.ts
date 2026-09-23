/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { homedir, userInfo } from "node:os";
import { redactSecrets, sanitizeContent, sanitizeValue } from "../lib/sanitize.js";

// Cases shared with the Rust log redactor (crates/aft/src/log_redact.rs) so the
// daemon's log files and `aft doctor --issue` mask secrets identically.
const sharedRedactionCases = JSON.parse(
  readFileSync(
    new URL("../../../../crates/aft/tests/fixtures/log_redaction_cases.json", import.meta.url),
    "utf8",
  ),
) as { masked: Array<{ input: string; expected: string }>; untouched: string[] };

describe("redactSecrets shared fixture", () => {
  test("masks every shared case exactly as the Rust redactor does", () => {
    expect(sharedRedactionCases.masked.length).toBeGreaterThan(0);
    for (const { input, expected } of sharedRedactionCases.masked) {
      expect(redactSecrets(input)).toBe(expected);
    }
  });

  test("leaves every shared untouched case unchanged", () => {
    expect(sharedRedactionCases.untouched.length).toBeGreaterThan(0);
    for (const input of sharedRedactionCases.untouched) {
      expect(redactSecrets(input)).toBe(input);
    }
  });
});

describe("sanitizeContent", () => {
  const originalHome = homedir();
  const originalUser = userInfo().username;

  test("replaces home directory with ~", () => {
    const input = `Error at ${originalHome}/foo/bar`;
    const out = sanitizeContent(input);
    expect(out).not.toContain(originalHome);
    expect(out).toContain("~/foo/bar");
  });

  test("replaces macOS /Users/<name>/ with <USER>", () => {
    const input = "Reading /Users/alice/.config/opencode/aft.jsonc";
    const out = sanitizeContent(input);
    expect(out).not.toContain("/Users/alice");
    expect(out).toContain("/Users/<USER>");
  });

  test("redacts Windows user paths across drives and slash styles", () => {
    // Other users / non-C: drives / forward-slash forms aren't covered by the
    // active-home/username passes, so they must be matched structurally — they
    // would otherwise leak into a public doctor --issue body.
    const cases: Array<[string, string]> = [
      ["C:\\Users\\Alice\\project\\x.ts", "C:\\Users\\<USER>"],
      ["D:\\Users\\Bob\\repo", "D:\\Users\\<USER>"],
      ["C:/Users/Carol/code/y.ts", "C:/Users/<USER>"],
      ["E:/Users/Dave/app", "E:/Users/<USER>"],
    ];
    for (const [input, expectedPrefix] of cases) {
      const out = sanitizeContent(input);
      expect(out).toContain("<USER>");
      expect(out.startsWith(expectedPrefix)).toBe(true);
      expect(out).not.toContain("Alice");
      expect(out).not.toContain("Bob");
      expect(out).not.toContain("Carol");
      expect(out).not.toContain("Dave");
    }
  });

  test("replaces Linux /home/<name>/ with <USER>", () => {
    const input = "Reading /home/bob/.config/opencode/aft.jsonc";
    const out = sanitizeContent(input);
    expect(out).not.toContain("/home/bob");
    expect(out).toContain("/home/<USER>");
  });

  test("replaces standalone username occurrences", () => {
    // Only meaningful when the test runner actually has a username.
    if (!originalUser) return;
    const input = `Config for ${originalUser} loaded`;
    const out = sanitizeContent(input);
    expect(out).not.toContain(originalUser);
    expect(out).toContain("<USER>");
  });

  test("is idempotent", () => {
    const input = `at ${originalHome}/foo`;
    const once = sanitizeContent(input);
    const twice = sanitizeContent(once);
    expect(twice).toBe(once);
  });

  test("sanitizes issue-title-sized strings", () => {
    const input = `AFT issue: failure under ${originalHome}/secret-project`;
    const out = sanitizeContent(input);
    expect(out).not.toContain(originalHome);
    expect(out).toContain("~/secret-project");
  });

  test("redacts bearer, basic auth, and GitHub tokens", () => {
    const bearer = "Authorization: Bearer abc.def_1234567890-secret";
    const basic = "Authorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==";
    const proxyBasic = "Proxy-Authorization: Basic cHJveHk6c2VjcmV0";
    const github = "token=ghp_abcdefghijklmnopqrstuvwxyz0123456789ABCD";
    const githubFineGrained = "github_pat_11AA22BB33CC_44dd55ee66";

    const out = sanitizeContent([bearer, basic, proxyBasic, github, githubFineGrained].join("\n"));

    expect(out).toContain("Authorization: Bearer <REDACTED_SECRET>");
    expect(out).toContain("Authorization: Basic <REDACTED_SECRET>");
    expect(out).toContain("Proxy-Authorization: Basic <REDACTED_SECRET>");
    expect(out).not.toContain("abc.def_1234567890-secret");
    expect(out).not.toContain("QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
    expect(out).not.toContain("cHJveHk6c2VjcmV0");
    expect(out).not.toContain("ghp_abcdefghijklmnopqrstuvwxyz0123456789ABCD");
    expect(out).not.toContain("github_pat_11AA22BB33CC_44dd55ee66");
  });

  test("redacts standalone JWTs and AWS access key IDs", () => {
    const jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.sig";
    const awsAccessKeyId = "AKIAIOSFODNN7EXAMPLE";
    const input = [
      `payload={"jwt":"${jwt}"}`,
      `aws access key id ${awsAccessKeyId} appeared in logs`,
    ].join("\n");

    const out = sanitizeContent(input);

    expect(out).not.toContain(jwt);
    expect(out).not.toContain(awsAccessKeyId);
    expect(out).toContain('{"jwt":"<REDACTED_SECRET>"}');
    expect(out).toContain("aws access key id <REDACTED_SECRET>");
  });

  test("redacts env-style sensitive keys", () => {
    const input = [
      "OPENCODE_SERVER_PASSWORD=swordfish",
      "GITHUB_TOKEN: ghx_secret_value",
      "OPENAI_API_KEY='sk-test-secret'",
      'AFT_KEY="key-secret"',
      "LEGACY_PASSWD=legacy-secret",
      "SHORT_PWD=pwd-secret",
      "DB_CREDENTIAL=credential-secret",
    ].join("\n");

    const out = sanitizeContent(input);

    expect(out).toContain("OPENCODE_SERVER_PASSWORD=<REDACTED_SECRET>");
    expect(out).toContain("GITHUB_TOKEN: <REDACTED_SECRET>");
    expect(out).toContain("OPENAI_API_KEY='<REDACTED_SECRET>'");
    expect(out).toContain('AFT_KEY="<REDACTED_SECRET>"');
    expect(out).toContain("LEGACY_PASSWD=<REDACTED_SECRET>");
    expect(out).toContain("SHORT_PWD=<REDACTED_SECRET>");
    expect(out).toContain("DB_CREDENTIAL=<REDACTED_SECRET>");
    expect(out).not.toContain("swordfish");
    expect(out).not.toContain("ghx_secret_value");
    expect(out).not.toContain("sk-test-secret");
    expect(out).not.toContain("key-secret");
    expect(out).not.toContain("legacy-secret");
    expect(out).not.toContain("pwd-secret");
    expect(out).not.toContain("credential-secret");
  });

  test("redacts quoted sensitive object keys", () => {
    const input = [
      '{"password":"hunter2"}',
      '{"api_key": "json-secret"}',
      '{"serviceToken":"token-secret"}',
      "{'api-key': 'dash-secret'}",
    ].join("\n");

    const out = sanitizeContent(input);

    expect(out).toContain('{"password":"<REDACTED_SECRET>"}');
    expect(out).toContain('{"api_key": "<REDACTED_SECRET>"}');
    expect(out).toContain('{"serviceToken":"<REDACTED_SECRET>"}');
    expect(out).toContain("{'api-key': '<REDACTED_SECRET>'}");
    expect(out).not.toContain("hunter2");
    expect(out).not.toContain("json-secret");
    expect(out).not.toContain("token-secret");
    expect(out).not.toContain("dash-secret");
  });

  test("leaves normal prose and incomplete credential labels unchanged", () => {
    const input = [
      "Please enter your password before retrying.",
      "The api_key setting name is documented here.",
      "monkey=banana",
      "keyboard_layout=us",
      "Authorization: Bearer",
      "Proxy-Authorization: Basic",
      "github_pat_ appears only as a prefix in this help text.",
    ].join("\n");

    expect(sanitizeContent(input)).toBe(input);
  });

  test("redacts large log tails without pathological backtracking", () => {
    const logTail = Array.from(
      { length: 50 },
      (_, index) =>
        `INFO ${index} password mentioned without a value and token label only ${"x".repeat(1000)}`,
    ).join("\n");
    const input = [
      logTail,
      "OPENCODE_SERVER_PASSWORD=swordfish",
      '{"password":"hunter2"}',
      "Authorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==",
      "Proxy-Authorization: Basic cHJveHk6c2VjcmV0",
      "github_pat_11AA22BB33CC_44dd55ee66",
      logTail,
    ].join("\n");

    const startedAt = Date.now();
    const out = sanitizeContent(input);

    expect(Date.now() - startedAt).toBeLessThan(1000);
    expect(input.length).toBeGreaterThan(50_000);
    expect(out).not.toContain("swordfish");
    expect(out).not.toContain("hunter2");
    expect(out).not.toContain("QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
    expect(out).not.toContain("cHJveHk6c2VjcmV0");
    expect(out).not.toContain("github_pat_11AA22BB33CC_44dd55ee66");
  });

  test("redacts common credentials, URL userinfo, and emails", () => {
    const input = [
      "api_key=sk-live-abcdefghijklmnopqrstuvwxyz123456",
      "password: hunter2",
      "remote=https://alice:swordfish@example.com/repo.git",
      "contact alice@example.com",
    ].join("\n");

    const out = sanitizeContent(input);

    expect(out).not.toContain("sk-live-abcdefghijklmnopqrstuvwxyz123456");
    expect(out).not.toContain("hunter2");
    expect(out).toContain("api_key=<REDACTED_SECRET>");
    expect(out).toContain("password: <REDACTED_SECRET>");
    expect(out).toContain("https://***@example.com/repo.git");
    expect(out).toContain("contact <EMAIL>");
  });
});

describe("sanitizeValue", () => {
  test("walks nested objects and arrays", () => {
    const input = {
      logs: [`line1 ${homedir()}/x`, `line2 ${homedir()}/y`],
      nested: {
        path: `${homedir()}/config/file.jsonc`,
        keep: 42,
      },
    };
    const out = sanitizeValue(input) as typeof input;
    expect(out.logs[0]).not.toContain(homedir());
    expect(out.logs[0]).toContain("~/x");
    expect(out.nested.path).not.toContain(homedir());
    expect(out.nested.keep).toBe(42);
  });

  test("preserves primitives", () => {
    expect(sanitizeValue(null)).toBeNull();
    expect(sanitizeValue(undefined)).toBeUndefined();
    expect(sanitizeValue(123)).toBe(123);
    expect(sanitizeValue(true)).toBe(true);
  });
});
