#!/usr/bin/env node
// Release-notes renderer check.
//
// GitHub renders release bodies with `breaks: true`, which is NOT how it
// renders README markdown. Under that setting every single newline inside a
// paragraph becomes a literal <br>, so a paragraph hard-wrapped at 80 columns
// renders as a column of ragged short lines. It looks fine in an editor and
// fine in most previews; it only shows up on the published release page, which
// is the one place nobody looks until a user does.
//
// v0.57.1 shipped that way. This check exists so the next one cannot.
//
// The rule is exact rather than heuristic: a prose paragraph must occupy a
// single source line. Constructs where a newline is itself meaningful markup
// (tables, lists, headings, code fences, blockquotes, raw HTML) are exempt,
// because their line breaks survive rendering as structure rather than <br>.
//
// Usage: node scripts/check-release-notes.mjs [--fix] <file>...

import { readFileSync, writeFileSync } from "node:fs";

// Lines that are block-level markup, where a newline is structural and so
// carries meaning through the renderer rather than degrading into a <br>.
function isStructuralLine(line) {
  const t = line.trim();
  if (t === "") return true;
  return (
    t.startsWith("#") || // heading
    t.startsWith("|") || // table row
    t.startsWith(">") || // blockquote
    t.startsWith("<") || // raw HTML
    /^[-*+]\s/.test(t) || // bullet list item
    /^\d+[.)]\s/.test(t) || // ordered list item
    /^(-{3,}|\*{3,}|_{3,})$/.test(t) || // horizontal rule
    /^\[[^\]]+\]:\s/.test(t) // link reference definition
  );
}

function findWrappedParagraphs(text) {
  const lines = text.split("\n");
  const findings = [];
  let fence = null; // active code-fence marker, if any
  let run = []; // consecutive prose lines = one paragraph

  const flush = () => {
    // A prose paragraph spanning 2+ source lines is hard-wrapped, and every
    // one of those breaks renders as a <br>.
    if (run.length > 1) {
      findings.push({ start: run[0].n, end: run[run.length - 1].n, lines: run.length });
    }
    run = [];
  };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const t = line.trim();

    const fenceMatch = t.match(/^(```+|~~~+)/);
    if (fenceMatch) {
      // Closing fence must match the marker style that opened the block, so a
      // ``` inside a ~~~ block does not end it early.
      if (fence === null) {
        flush();
        fence = fenceMatch[1][0];
      } else if (fenceMatch[1][0] === fence) {
        fence = null;
      }
      continue;
    }
    if (fence !== null) continue; // inside code: verbatim, never our business

    if (isStructuralLine(line)) {
      flush();
      continue;
    }
    run.push({ n: i + 1, text: line });
  }
  flush();
  return findings;
}

// Join each hard-wrapped prose paragraph onto one line. Only whitespace at the
// join points changes; no word is added, removed, or reordered, which the
// caller verifies by comparing word streams before and after.
function reflow(text) {
  const lines = text.split("\n");
  const out = [];
  let fence = null;
  let run = [];

  const flush = () => {
    if (run.length > 0) {
      out.push(run.map((l) => l.trim()).join(" "));
      run = [];
    }
  };

  for (const line of lines) {
    const t = line.trim();
    const fenceMatch = t.match(/^(```+|~~~+)/);
    if (fenceMatch) {
      if (fence === null) {
        flush();
        fence = fenceMatch[1][0];
      } else if (fenceMatch[1][0] === fence) {
        fence = null;
      }
      out.push(line);
      continue;
    }
    if (fence !== null) {
      out.push(line);
      continue;
    }
    if (isStructuralLine(line)) {
      flush();
      out.push(line);
      continue;
    }
    run.push(line);
  }
  flush();
  return out.join("\n");
}

const argv = process.argv.slice(2);
const fix = argv.includes("--fix");
const files = argv.filter((a) => a !== "--fix");
if (files.length === 0) {
  console.error("usage: check-release-notes.mjs [--fix] <file>...");
  process.exit(2);
}

let failed = false;
for (const file of files) {
  let text;
  try {
    text = readFileSync(file, "utf8");
  } catch (err) {
    console.error(`Error: cannot read ${file}: ${err.message}`);
    failed = true;
    continue;
  }

  const findings = findWrappedParagraphs(text);
  if (findings.length === 0) {
    console.log(`  ok  ${file}`);
    continue;
  }

  if (fix) {
    const fixed = reflow(text);
    // Refuse to write a repair that changed any word: the transform is only
    // ever allowed to move whitespace.
    const words = (s) => s.split(/\s+/).filter(Boolean).join("\n");
    if (words(fixed) !== words(text)) {
      console.error(`Error: ${file}: reflow would alter words, refusing to write.`);
      failed = true;
      continue;
    }
    if (findWrappedParagraphs(fixed).length !== 0) {
      console.error(`Error: ${file}: reflow did not settle, refusing to write.`);
      failed = true;
      continue;
    }
    writeFileSync(file, fixed);
    console.log(`  fixed  ${file} (${findings.length} paragraph(s) reflowed)`);
    continue;
  }

  failed = true;
  console.error(`\nError: ${file} has ${findings.length} hard-wrapped paragraph(s).`);
  console.error("  GitHub renders release bodies with breaks:true, so every newline inside");
  console.error("  a paragraph becomes a <br> and the paragraph renders as ragged short lines.");
  for (const f of findings) {
    console.error(`    lines ${f.start}-${f.end} (${f.lines} lines, should be 1)`);
  }
  console.error("  → Reflow each paragraph onto a single line. Blank lines still separate");
  console.error("    paragraphs; tables, lists, headings and code blocks are unaffected.");
}

process.exit(failed ? 1 : 0);
