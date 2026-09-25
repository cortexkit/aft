import type { Readable, Writable } from "node:stream";
import { styleText } from "node:util";
import { GroupMultiSelectPrompt } from "@clack/core";
import {
  formatInstructionFooter,
  isCancel,
  limitOptions,
  MULTISELECT_INSTRUCTIONS,
  S_BAR,
  S_BAR_END,
  S_CHECKBOX_ACTIVE,
  S_CHECKBOX_INACTIVE,
  S_CHECKBOX_SELECTED,
  symbol,
} from "@clack/prompts";

/**
 * The grouped feature checklist for setup.
 *
 * Clack's own group multiselect prints a row's description as a hint in
 * parentheses after the label, only for checked or focused rows, and wraps a
 * long hint with the wrong tree prefix (the continuation line loses the `│`
 * that joins the group). This renderer puts each description on its own
 * indented line under the label, word-wrapped to the terminal width with the
 * same tree prefix on every line, so a row renders the same whatever its state
 * and the tree never breaks. Rows show only the label and description.
 */

export interface FeatureRow {
  value: string;
  label: string;
  description: string;
}

/** Everything drawn before a row's text: the prompt guide bar and the tree column. */
const GUIDE = `${S_BAR}  `;
/** Row text column: guide (3) + tree (2) + checkbox (2). */
const TEXT_INDENT = 7;
const MIN_TEXT_WIDTH = 20;

const dim = (text: string) => styleText("dim", text);

/** Word-wrap plain text to `width` columns; a word longer than a line is split. */
export function wrapWords(text: string, width: number): string[] {
  const lines: string[] = [];
  let line = "";
  for (const word of text.split(/\s+/).filter(Boolean)) {
    let rest = word;
    while (rest.length > width) {
      if (line) {
        lines.push(line);
        line = "";
      }
      lines.push(rest.slice(0, width));
      rest = rest.slice(width);
    }
    if (!line) line = rest;
    else if (line.length + 1 + rest.length <= width) line = `${line} ${rest}`;
    else {
      lines.push(line);
      line = rest;
    }
  }
  if (line) lines.push(line);
  return lines;
}

export type RowState = "active" | "selected" | "active-selected" | "inactive";

function checkbox(state: RowState): string {
  if (state === "active-selected" || state === "selected") {
    return styleText("green", S_CHECKBOX_SELECTED);
  }
  if (state === "active") return styleText("cyan", S_CHECKBOX_ACTIVE);
  return dim(S_CHECKBOX_INACTIVE);
}

/**
 * One feature row as terminal lines, without the guide bar. `last` marks the
 * final row of its group, which closes the tree branch.
 */
export function renderRowLines(
  row: FeatureRow,
  state: RowState,
  last: boolean,
  columns: number,
): string[] {
  const branch = dim(last ? S_BAR_END : S_BAR);
  const continuation = last ? " " : dim(S_BAR);
  const focused = state === "active" || state === "active-selected";
  // Only the focused row's label is at full brightness, as in clack's own
  // lists; the checkbox colour shows checked or not. A checked row drawn at
  // full brightness would hide the cursor whenever it sits on a checked row.
  const label = focused ? row.label : dim(row.label);
  const lines = [`${branch} ${checkbox(state)} ${label}`];
  const width = Math.max(MIN_TEXT_WIDTH, columns - TEXT_INDENT - 1);
  for (const text of wrapWords(row.description, width)) {
    lines.push(`${continuation}   ${dim(text)}`);
  }
  return lines;
}

/** A group header: the group name with a checkbox that reflects all its rows. */
export function renderGroupLine(name: string, state: RowState): string {
  return `${checkbox(state)} ${state === "inactive" ? dim(name) : name}`;
}

/**
 * A complete static frame of the checklist: message, every group and row, and
 * the key help. Used for the prompt's first paint and by tests; the live
 * prompt scrolls the same lines when they do not fit the terminal.
 */
export function renderFeatureList(
  message: string,
  groups: Record<string, FeatureRow[]>,
  selected: ReadonlySet<string>,
  columns: number,
  cursor: string | null = null,
): string {
  const out = [S_BAR, `${symbol("active")}  ${message}`];
  for (const [group, rows] of Object.entries(groups)) {
    const groupSelected = rows.length > 0 && rows.every((row) => selected.has(row.value));
    out.push(`${GUIDE}${renderGroupLine(group, rowState(group === cursor, groupSelected))}`);
    rows.forEach((row, index) => {
      const state = rowState(row.value === cursor, selected.has(row.value));
      for (const line of renderRowLines(row, state, index === rows.length - 1, columns)) {
        out.push(`${GUIDE}${line}`);
      }
    });
  }
  out.push(...formatInstructionFooter(MULTISELECT_INSTRUCTIONS, true));
  return out.join("\n");
}

function rowState(active: boolean, selected: boolean): RowState {
  if (active) return selected ? "active-selected" : "active";
  return selected ? "selected" : "inactive";
}

type PromptOption = FeatureRow & { group: string | boolean };

/** Terminal streams for the prompt; tests pass their own to render at a fixed width. */
export interface PromptStreams {
  input?: Readable;
  output?: Writable & { columns?: number };
}

/** Show the checklist and return the checked row values; calls `onCancel` on cancel. */
export async function promptFeatureList(
  message: string,
  groups: Record<string, FeatureRow[]>,
  initial: string[],
  onCancel: () => never,
  streams: PromptStreams = {},
): Promise<string[]> {
  const output = streams.output ?? process.stdout;
  const columnsOf = () => output.columns ?? 80;
  const rowsByValue = new Map<string, { row: FeatureRow; last: boolean }>();
  for (const rows of Object.values(groups)) {
    rows.forEach((row, index) => {
      rowsByValue.set(row.value, { row, last: index === rows.length - 1 });
    });
  }
  const prompt = new GroupMultiSelectPrompt<FeatureRow>({
    options: groups,
    initialValues: initial,
    required: false,
    selectableGroups: true,
    ...(streams.input ? { input: streams.input } : {}),
    ...(streams.output ? { output: streams.output } : {}),
    render() {
      const header = `${S_BAR}\n${symbol(this.state)}  ${message}\n`;
      const values = (this.value ?? []) as string[];
      if (this.state === "submit" || this.state === "cancel") {
        const picked = this.options
          .filter((option) => option.group !== true && values.includes(option.value))
          .map((option) => option.label);
        const summary =
          this.state === "cancel"
            ? styleText(["strikethrough", "dim"], picked.join(", ") || "none")
            : dim(picked.join(", ") || "none");
        const columns = columnsOf();
        const wrapped = wrapWords(summary, Math.max(MIN_TEXT_WIDTH, columns - GUIDE.length));
        return `${header}${wrapped.map((line) => `${GUIDE}${line}`).join("\n")}`;
      }
      const columns = columnsOf();
      const styled = (option: PromptOption, active: boolean) => {
        if (option.group === true) {
          const name = String(option.value);
          return renderGroupLine(name, rowState(active, this.isGroupSelected(name)));
        }
        const entry = rowsByValue.get(option.value);
        if (!entry) return option.label;
        const state = rowState(active, values.includes(option.value));
        return renderRowLines(entry.row, state, entry.last, columns).join("\n");
      };
      const footer = formatInstructionFooter(MULTISELECT_INSTRUCTIONS, true);
      const lines = limitOptions({
        options: this.options as PromptOption[],
        cursor: this.cursor,
        output,
        columnPadding: GUIDE.length,
        rowPadding: header.split("\n").length + footer.length + 1,
        style: styled,
      });
      return `${header}${GUIDE}${lines.join(`\n${GUIDE}`)}\n${footer.join("\n")}\n`;
    },
  }).prompt();
  const result = await prompt;
  if (isCancel(result)) onCancel();
  return result as string[];
}
