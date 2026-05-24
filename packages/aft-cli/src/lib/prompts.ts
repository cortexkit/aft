import {
  confirm as clackConfirm,
  text as clackText,
  intro,
  isCancel,
  log,
  multiselect,
  note,
  outro,
  select,
  spinner,
} from "@clack/prompts";

export { intro, log, note, outro, spinner };

function handleCancel(value: unknown, message = "Cancelled."): void {
  if (isCancel(value)) {
    log.warn(message);
    process.exit(0);
  }
}

export async function confirm(message: string, defaultYes = true): Promise<boolean> {
  const result = await clackConfirm({
    message,
    initialValue: defaultYes,
  });
  handleCancel(result);
  return result as boolean;
}

/**
 * A select option; `T` is the string literal union you'd like returned. The
 * `value` is stored/returned as that narrow literal even though we hand Clack
 * a concrete `string` at runtime.
 */
interface PromptOption<T extends string> {
  label: string;
  value: T;
  hint?: string;
  recommended?: boolean;
}

export async function selectOne<T extends string>(
  message: string,
  options: PromptOption<T>[],
): Promise<T> {
  const clackOptions = options.map((option) => {
    const hint = option.hint ?? (option.recommended ? "recommended" : undefined);
    const label = option.recommended ? `${option.label} (recommended)` : option.label;
    return hint === undefined
      ? { label, value: option.value as string }
      : { label, value: option.value as string, hint };
  });
  const result = await select({ message, options: clackOptions });
  handleCancel(result);
  return result as T;
}

export async function selectMany<T extends string>(
  message: string,
  options: PromptOption<T>[],
  initialValues?: T[],
  required = true,
): Promise<T[]> {
  const clackOptions = options.map((option) =>
    option.hint === undefined
      ? { label: option.label, value: option.value as string }
      : { label: option.label, value: option.value as string, hint: option.hint },
  );
  const result = await multiselect({
    message,
    options: clackOptions,
    ...(initialValues ? { initialValues: initialValues as string[] } : {}),
    required,
  });
  handleCancel(result);
  return result as T[];
}

export async function text(
  message: string,
  options: {
    placeholder?: string;
    defaultValue?: string;
    validate?: (value: string) => string | Error | undefined;
  } = {},
): Promise<string> {
  const promptOptions: Parameters<typeof clackText>[0] = {
    message,
    ...(options.placeholder ? { placeholder: options.placeholder } : {}),
    ...(options.defaultValue !== undefined ? { defaultValue: options.defaultValue } : {}),
    ...(options.validate
      ? {
          validate: (value) => options.validate?.(value ?? ""),
        }
      : {}),
  };
  const result = await clackText(promptOptions);
  handleCancel(result);
  return result as string;
}
