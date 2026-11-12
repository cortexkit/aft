import { describe, expect, test } from "bun:test";
import { projectRootsFromValue, sanitizeContent, sanitizeValue } from "../lib/sanitize.js";

const context = { home: "/home/dev", username: "dev" };
const root = "/projects/alice/repo";
const encoded = (text: string) => encodeURIComponent(text);
const lowerEncoded = (text: string) =>
  encoded(text).replace(/%[\dA-F]{2}/g, (hex) => hex.toLowerCase());

describe("issue report path privacy", () => {
  test("collects configured, session and status roots without treating other paths as projects", () => {
    const value = {
      index: {
        roots: [
          { path: root, indexes: ["search"] },
          { path: "~/private-project", indexes: ["search"] },
        ],
      },
      sessions: [{ projectRoot: "/projects/bob/other" }],
      status: { project_root: "/projects/carol/worktree", canonical_root: "/projects/carol/main" },
      configPath: "/etc/aft.jsonc",
    };
    const roots = projectRootsFromValue(value);
    expect(roots).toEqual([
      root,
      "~/private-project",
      "/projects/bob/other",
      "/projects/carol/worktree",
      "/projects/carol/main",
    ]);
    expect(
      sanitizeContent(`${root}/src /home/dev/private-project/src ~/private-project/src`, {
        ...context,
        projectRoots: roots,
      }),
    ).toBe("<PROJECT>/src <PROJECT>/src <PROJECT>/src");
    const output = JSON.stringify(sanitizeValue(value, context));
    expect(output).not.toContain("alice");
    expect(output).not.toContain("private-project");
    expect(output).toContain("/etc/aft.jsonc");
    expect(sanitizeContent("src/main.ts", { ...context, projectRoots: ["src"] })).toBe(
      "src/main.ts",
    );
  });

  test("redacts every bound project across all report sections, longest first", () => {
    const input = [
      `## Description\nFailure at ${root}/src/main.ts`,
      `## Diagnostics\nproject root set: ${root}/nested`,
      `## Recent errors\nERROR root=${root} cannot read ${root}/src/main.ts`,
      `## Logs (last 200 lines per source)\nwatcher started: ${root}/nested`,
      `root=/projects/bob/other cmd=cd ${root}/nested && cat src/main.ts`,
      `location%5Bdirectory%5D=${encoded(root)}`,
      `location%5bdirectory%5d=${lowerEncoded(root)}`,
      `project root set: /projects/carol/a spaced repo`,
      `watcher started: /projects/carol/a spaced repo/nested project`,
      `location=${encoded("/projects/carol/a spaced repo")}`,
    ].join("\n");
    const output = sanitizeContent(input, context);
    expect(output).not.toContain("alice");
    expect(output).not.toContain("bob");
    expect(output).not.toContain("carol");
    expect(output).not.toContain("spaced");
    expect(output).not.toContain("nested project");
    expect(output).not.toContain("<PROJECT>/nested");
    expect(output).toContain("Failure at <PROJECT>/src/main.ts");
    expect(output).toContain("cmd=cd <PROJECT> && cat src/main.ts");
    expect(output).toContain("location%5Bdirectory%5D=<PROJECT>");
    expect(output).toContain("location%5bdirectory%5d=<PROJECT>");
    expect(sanitizeContent(output, context)).toBe(output);
  });

  test("redacts supplied roots even when cwd is elsewhere and respects path boundaries", () => {
    const input = `${root}/src ${root}-public/src docs${root}/src .${root}/src ..${root}/src src/main.ts`;
    expect(sanitizeContent(input, { ...context, projectRoots: [root] })).toBe(
      `<PROJECT>/src ${root}-public/src docs${root}/src .${root}/src ..${root}/src src/main.ts`,
    );
    expect(
      sanitizeContent(`file://${root}/src file:///home/dev/config`, {
        ...context,
        projectRoots: [root],
      }),
    ).toBe("file://<PROJECT>/src file://~/config");
  });

  test("redacts home paths in cache, storage and config in plain, escaped and encoded forms", () => {
    for (const suffix of [
      "/.cache/aft/bin/aft",
      "/.local/share/cortexkit/aft/logs",
      "/.config/cortexkit/aft.jsonc",
    ]) {
      const path = context.home + suffix;
      expect(sanitizeContent(path, context)).toBe(`~${suffix}`);
      expect(sanitizeContent(path.replaceAll("/", "\\/"), context)).toBe(
        `~${suffix.replaceAll("/", "\\/")}`,
      );
      expect(sanitizeContent(encoded(path), context)).toBe(`~${encoded(suffix)}`);
      expect(sanitizeContent(lowerEncoded(path), context)).toBe(`~${lowerEncoded(suffix)}`);
    }
  });

  test("decodes mixed encoding, Unicode and JSON-escaped project roots without rewriting the rest", () => {
    const unicodeRoot = "/projects/alice/café";
    const options = { ...context, projectRoots: [root, unicodeRoot] };
    expect(sanitizeContent(`${root.replaceAll("/", "\\/")}\\/src`, options)).toBe(
      "<PROJECT>\\/src",
    );
    expect(sanitizeContent("%2fprojects%2Falice/repo%2Fsrc%2fmain.ts", options)).toBe(
      "<PROJECT>%2Fsrc%2fmain.ts",
    );
    expect(sanitizeContent(`${encoded(unicodeRoot)}%2Fsrc`, options)).toBe("<PROJECT>%2Fsrc");
    expect(sanitizeContent("unchanged%20text %FF %2 src/relative.ts", options)).toBe(
      "unchanged%20text %FF %2 src/relative.ts",
    );
  });

  test("handles Windows homes and projects with plain, JSON and either percent hex case", () => {
    const home = "D:\\Users\\dev";
    const project = `${home}\\projects\\repo`;
    const options = { home, username: "dev", projectRoots: [project] };
    for (const spelling of [
      project,
      project.replaceAll("\\", "\\\\"),
      encoded(project),
      lowerEncoded(project),
      project.replaceAll("\\", "/"),
    ]) {
      expect(sanitizeContent(spelling, options)).toBe("<PROJECT>");
    }
    for (const spelling of [
      home,
      home.replaceAll("\\", "\\\\"),
      encoded(home),
      lowerEncoded(home),
    ]) {
      expect(sanitizeContent(`${spelling}/cache`, options)).toBe("~/cache");
    }
    expect(sanitizeContent("E:%5cUsers%5cother%5crepo", options)).toBe(
      "E:%5cUsers%5c<USER>%5crepo",
    );
    expect(sanitizeContent("\\/home\\/other\\/cache", options)).toBe("\\/home\\/<USER>\\/cache");
  });

  test("never reveals a common username by modifying system paths or relative paths", () => {
    const input = [
      "2>/dev/null /proc/dev/status /sys/dev /usr/dev /etc/dev /bin/dev /var/dev /tmp/dev /opt/dev /sbin/dev /run/dev",
      "dev build src/dev/main.ts ./dev/null relative/home/dev/file docs/Users/dev/file ./home/dev/file ../home/dev/file ./Users/dev/file",
      "%2Fdev%2Fnull \\/dev\\/null C:\\dev\\null",
      "src/main.ts unchanged%20text",
    ].join("\n");
    expect(sanitizeContent(input, context)).toBe(input);
    expect(sanitizeContent("dev@host ~dev /home/dev/cache", context)).toBe(
      "<USER>@host ~<USER> ~/cache",
    );
    expect(sanitizeContent("%64%65%76@host ~%64%65%76", context)).toBe("<USER>@host ~<USER>");
  });

  test("redacts distinctive username words in prose, paths and assignments in every spelling", () => {
    const options = { home: "/home/alicejones", username: "alicejones" };
    expect(
      sanitizeContent(
        "alicejones logged in owner=alicejones /mnt/data/alicejones/x src/alicejones/main.ts ~alicejones alicejones@host",
        options,
      ),
    ).toBe(
      "<USER> logged in owner=<USER> /mnt/data/<USER>/x src/<USER>/main.ts ~<USER> <USER>@host",
    );
    const cases = [
      ["/mnt/data/alicejones/x", "/mnt/data/<USER>/x"],
      ["\\/mnt\\/data\\/alicejones\\/x", "\\/mnt\\/data\\/<USER>\\/x"],
      ["%2Fmnt%2Fdata%2Falicejones%2Fx", "%2Fmnt%2Fdata%2F<USER>%2Fx"],
      ["%2fmnt%2fdata%2falicejones%2fx", "%2fmnt%2fdata%2f<USER>%2fx"],
      ["D:\\\\data\\\\alicejones\\\\x", "D:\\\\data\\\\<USER>\\\\x"],
      ["D%3A%5Cdata%5Calicejones%5Cx", "D%3A%5Cdata%5C<USER>%5Cx"],
      ["D%3a%5cdata%5calicejones%5cx", "D%3a%5cdata%5c<USER>%5cx"],
    ];
    for (const [input, expected] of cases) {
      expect(sanitizeContent(input, options)).toBe(expected);
    }
    expect(sanitizeContent("owner=%61licejones %61%6c%69%63%65%6a%6f%6e%65%73", options)).toBe(
      "owner=<USER> <USER>",
    );
    const unchanged = "alicejones2 xalicejones alicejones_name unchanged%20text src/main.ts";
    expect(sanitizeContent(unchanged, options)).toBe(unchanged);
  });

  test("preserves distinctive username components in well-known system paths in every spelling", () => {
    const options = { username: "alicejones" };
    for (const directory of [
      "dev",
      "proc",
      "sys",
      "usr",
      "etc",
      "bin",
      "sbin",
      "var",
      "tmp",
      "opt",
      "run",
      "lib",
    ]) {
      const path = `/${directory}/nested/alicejones/file`;
      for (const spelling of [
        path,
        path.replaceAll("/", "\\/"),
        path.replaceAll("/", "\\\\"),
        encoded(path),
        lowerEncoded(path),
        encoded(path).replace("alicejones", "%61licejones"),
        `file://${path}`,
        `C:${path.replaceAll("/", "\\")}`,
      ]) {
        expect(sanitizeContent(spelling, options)).toBe(spelling);
      }
    }
    expect(sanitizeContent("/dev/null;owner=alicejones /device/alicejones", options)).toBe(
      "/dev/null;owner=<USER> /device/<USER>",
    );
  });

  test("treats common account names case-insensitively and redacts only their identities", () => {
    for (const username of [
      "root",
      "admin",
      "administrator",
      "user",
      "dev",
      "developer",
      "test",
      "guest",
      "ubuntu",
      "debian",
      "pi",
      "runner",
      "vagrant",
      "ec2-user",
      "centos",
      "git",
      "docker",
      "build",
      "ci",
      "DEV",
      "Admin",
    ]) {
      const options = { username, home: "/home/someone-else" };
      const ordinary = `${username} owner=${username} /mnt/data/${username}/x src/${username}/file /dev/null`;
      expect(sanitizeContent(ordinary, options)).toBe(ordinary);
      expect(sanitizeContent(`~${username} ${username}@host /home/${username}/file`, options)).toBe(
        "~<USER> <USER>@host /home/<USER>/file",
      );
    }
  });
});
