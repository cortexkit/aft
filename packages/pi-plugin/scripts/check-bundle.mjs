import ts from "typescript";

const TUI_SPECIFIER = /^@(?:earendil-works|oh-my-pi)\/pi-tui(?:\/|$)/;

export function bundledTuiSpecifiers(source) {
  const file = ts.createSourceFile("index.js", source, ts.ScriptTarget.Latest, true);
  const specifiers = new Set();
  function visit(node) {
    // Check literals rather than call names: Bun can rename the generated require helper.
    if (ts.isStringLiteralLike(node) && TUI_SPECIFIER.test(node.text)) specifiers.add(node.text);
    ts.forEachChild(node, visit);
  }
  visit(file);
  return [...specifiers];
}

if (import.meta.main) {
  const bundle = Bun.file(new URL("../dist/index.js", import.meta.url));
  const specifiers = bundledTuiSpecifiers(await bundle.text());
  if (specifiers.length > 0) {
    throw new Error(`dist/index.js must not load a host TUI package: ${specifiers.join(", ")}`);
  }
  console.log("Checked dist/index.js: no host TUI module specifiers");
}
