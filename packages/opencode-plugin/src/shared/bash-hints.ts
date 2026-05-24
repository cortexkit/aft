// Pure helpers for the bash-output hint nudges fired from `tool.execute.after`.
//
// These run on every bash result and append agent-visible "[Hint] ..." lines
// that steer toward AFT tools. The functions return the new output string
// (or the original string when no hint should fire) so the hook can compose
// them without managing state.

const CONFLICT_HINT =
  "\n\n[Hint] Use aft_conflicts to see all conflict regions across files in a single call.";
const GREP_HINT = "\n\n[Hint] Use the grep tool instead of bash for faster indexed search.";

/**
 * Append the `aft_conflicts` hint when the output indicates a real git merge
 * or rebase produced conflicts.
 *
 * Gated on BOTH:
 *  - the "Automatic merge failed; fix conflicts" marker, AND
 *  - a git-conflict signal (`CONFLICT (...)` line or `error: could not apply`)
 *
 * Both conditions are required because `aft_conflicts` calls `git ls-files -u`,
 * which fails with "not a git repository" outside a git working tree. The
 * marker string can legitimately appear in docs, READMEs, test fixtures, and
 * grep output, so we cannot key off it alone — a false-positive hint sends
 * agents into a confusing error.
 */
export function maybeAppendConflictsHint(output: string): string {
  if (!output.includes("Automatic merge failed; fix conflicts")) return output;
  // git merge prints "CONFLICT (content|file|...): ..." per file.
  // git rebase / git am print "error: could not apply <sha>" per failed pick.
  if (!/^CONFLICT \(|^error: could not apply /m.test(output)) return output;
  return output + CONFLICT_HINT;
}

/**
 * Append the grep-tool hint when the bash command was a grep/rg invocation.
 *
 * Detected by examining the FIRST LINE of output (which is the echoed command
 * line on most shells in foreground mode). We cap the slice at 300 chars so
 * a single huge first line doesn't slow this hook.
 */
export function maybeAppendGrepHint(output: string): string {
  const firstLine = output.slice(0, 300).split("\n")[0] ?? "";
  if (!/\b(rg|grep)\s/.test(firstLine)) return output;
  return output + GREP_HINT;
}
