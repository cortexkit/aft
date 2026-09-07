/**
 * Build a child environment whose executable search path is `dir` followed by
 * the inherited path, with exactly one path-like key on Windows.
 *
 * Windows environment variables are case-insensitive but JavaScript object
 * keys are not. A parent typically spells the variable `Path`; spreading
 * `process.env` and then assigning `PATH` produces an object with BOTH keys,
 * and which one the child's environment block keeps is undefined - in the
 * field the AFT worker lost the inherited path and could not resolve `git`,
 * `pwsh`, or `bash` by name (#298). So on `win32` every case-variant is
 * removed and one key is written back under the inherited spelling (`PATH`
 * only when nothing was inherited). On other platforms `Path` and `path` are
 * distinct variables and are left alone; only the exact `PATH` is edited.
 *
 * With no `dir` the call is a pure normalization. The input is not mutated.
 */
export function withPathPrepended(
  env: NodeJS.ProcessEnv,
  dir?: string | null,
  platform: NodeJS.Platform = process.platform,
): NodeJS.ProcessEnv {
  const output = { ...env };

  if (platform !== "win32") {
    if (dir) {
      output.PATH = env.PATH ? `${dir}:${env.PATH}` : dir;
    }
    return output;
  }

  const inheritedKey = Object.keys(env).find((key) => key.toLowerCase() === "path");
  const inheritedValue = inheritedKey === undefined ? undefined : env[inheritedKey];

  for (const key of Object.keys(output)) {
    if (key.toLowerCase() === "path") delete output[key];
  }

  const pathValue = dir ? (inheritedValue ? `${dir};${inheritedValue}` : dir) : inheritedValue;
  if (pathValue !== undefined) {
    output[inheritedKey ?? "PATH"] = pathValue;
  }

  return output;
}
