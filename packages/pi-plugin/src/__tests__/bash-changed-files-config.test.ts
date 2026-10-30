import { expect, test } from "bun:test";
import { formatSystemReminder } from "../bg-notifications";
import { AftConfigSchema, resolveBashConfig, resolveProjectOverridesForConfigure } from "../config";

test("bash changed_files defaults off and accepts only supported modes", () => {
  expect(resolveBashConfig({}).changed_files).toBe("off");
  for (const changed_files of ["off", "list", "diff"] as const) {
    const config = AftConfigSchema.parse({ disabled_tools: [], bash: { changed_files } });
    expect(resolveBashConfig(config).changed_files).toBe(changed_files);
    expect(resolveProjectOverridesForConfigure(config).bash).toEqual({ changed_files });
  }
  expect(AftConfigSchema.safeParse({ bash: { changed_files: true } }).success).toBe(false);
  expect(AftConfigSchema.safeParse({ bash: { changed_files: "all" } }).success).toBe(false);
});

test("background completion renders changed files once after command output", () => {
  const text =
    "Files changed while the command ran (changes by other processes may be included):\n  added new.txt";
  const reminder = formatSystemReminder([
    {
      task_id: "changed-files",
      status: "completed",
      exit_code: 0,
      command: "generator",
      output_preview: "command output",
      _bash_changed_files: { text, files: [], incomplete: [] },
    },
  ]);
  expect(reminder.indexOf(text)).toBeGreaterThan(reminder.indexOf("command output"));
  expect(reminder.split(text)).toHaveLength(2);
});
