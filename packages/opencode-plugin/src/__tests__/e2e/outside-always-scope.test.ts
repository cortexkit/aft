import { describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, realpath, rm } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join, parse } from "node:path";
import { Effect } from "effect";
import { requestPermission } from "../../permissions/v2.js";
import type { V2PermissionRequest } from "../../tools/definitions/v2.js";
import { withOutsidePermissions } from "../../tools/outside-permissions.js";
import { decidePermission, type V2PermissionRule } from "../v2-permission-host.js";
import { configureParamsFromLegacyOverrides, createHarness, prepareBinary } from "./helpers.js";

const binary = await prepareBinary();

describe.skipIf(!binary.binaryPath)("outside-file always scope", () => {
  for (const version of [1, 2] as const) {
    test(`OpenCode ${version} saves only the file in broad folders, but keeps normal folder grants`, async () => {
      const cache = join(homedir(), ".cache");
      await mkdir(cache, { recursive: true });
      const environment = await mkdtemp(join(cache, "aft-always-scope-"));
      const h = await createHarness(binary, {
        childEnvironmentRoot: environment,
        fixtureNames: [],
        configOverrides: configureParamsFromLegacyOverrides({
          restrict_to_project_root: false,
          bash_permissions: true,
        }),
      });
      try {
        const home = await realpath(join(environment, "home"));
        const normal = join(home, "Work", "other-project", "src");
        await mkdir(normal, { recursive: true });
        const root = parse(home).root;
        const systemFolders =
          process.platform === "win32"
            ? [process.env.SystemRoot ?? join(root, "Windows")]
            : ["/etc", "/usr/local"];
        const folders = [
          { label: "home", directory: home, narrow: true },
          { label: "home ancestor", directory: dirname(home), narrow: true },
          { label: "root", directory: root, narrow: true },
          ...systemFolders.map((directory) => ({ label: directory, directory, narrow: true })),
          { label: "normal", directory: normal, narrow: false },
        ];
        for (const { label, directory, narrow } of folders) {
          const folder = await realpath(directory);
          const first = join(folder, "aft-always-first.txt");
          const sibling = join(folder, "aft-always-second.txt");
          const expected = narrow ? first : join(folder, "*");
          for (const operation of ["read", "write"] as const) {
            const rules: V2PermissionRule[] = [];
            const prompts: V2PermissionRequest[] = [];
            const approve = (request: V2PermissionRequest) => {
              if (decidePermission(request, rules).effect === "allow") return;
              prompts.push(request);
              rules.push(
                ...request.always.map((resource) => ({
                  action: request.permission,
                  resource,
                  effect: "allow" as const,
                })),
              );
            };
            const runtime = {
              ...(version === 2 ? { hostVersion: 2 as const } : {}),
              ask: async (request: V2PermissionRequest) => {
                if (version === 1) return approve(request);
                await requestPermission(
                  {
                    permission: {
                      assert: (input) =>
                        Effect.sync(() =>
                          approve({
                            permission: input.action,
                            patterns: [...input.resources],
                            always: [...input.save],
                            metadata: input.metadata ?? {},
                          }),
                        ),
                    },
                  },
                  request,
                  {
                    sessionID: "always-scope",
                    messageID: "message",
                    id: "call",
                    progress: () => Effect.void,
                  },
                );
              },
            };
            for (const file of [first, first, sibling]) {
              // Get a real permission-required response without approving a write to a system folder.
              const response = await h.bridge.toolCall(
                `scope-${version}-${label}-${operation}`,
                operation,
                {
                  path: file,
                  ...(operation === "write" ? { content: "not written" } : {}),
                },
              );
              expect(response.code, `${label}: ${operation}`).toBe("permission_required");
              let sent = false;
              await withOutsidePermissions(runtime, {}, async () => {
                if (sent) return { success: true };
                sent = true;
                return { ...response };
              });
            }
            expect(prompts[0]?.always, `${label}: ${operation} saved scope`).toEqual([expected]);
            expect(prompts[0]?.patterns, `${label}: ${operation} matching scope`).toEqual([
              expected,
            ]);
            expect(prompts.length, `${label}: ${operation} sibling prompts`).toBe(narrow ? 2 : 1);
            if (narrow) expect(prompts[1]?.always).toEqual([sibling]);
          }
          // Shell scans have their own external-directory ask construction.
          const shell = await h.bridge.send("bash", {
            session_id: `shell-${version}-${label}`,
            command: `cat "${first.replaceAll("\\", "/")}"`,
          });
          expect(shell.code).toBe("permission_required");
          const external = (
            shell.asks as Array<{ kind: string; patterns: string[]; always: string[] }>
          ).filter((ask) => ask.kind === "external_directory");
          expect(external).toHaveLength(1);
          const shellExpected = narrow ? first : `${folder}/*`;
          expect(external[0]?.always).toEqual([shellExpected]);
          expect(external[0]?.patterns).toEqual([shellExpected]);
        }
      } finally {
        await h.cleanup();
        await rm(environment, { recursive: true, force: true });
      }
    }, 60_000);
  }
});
