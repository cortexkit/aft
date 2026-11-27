import ts from "typescript";

const TUI_SPECIFIER = /^@(?:earendil-works|oh-my-pi)\/pi-tui(?:\/|$)/;
// Bun emits __require for external CommonJS imports in an ESM bundle.
const REQUIRE_CALLEE = /^(?:require|__require\d*)$/;

export function bundledTuiSpecifiers(source) {
  const file = ts.createSourceFile("index.js", source, ts.ScriptTarget.Latest, true);
  const specifiers = new Set();
  function check(node) {
    if (node && ts.isStringLiteralLike(node) && TUI_SPECIFIER.test(node.text)) {
      specifiers.add(node.text);
    }
  }
  function visit(node) {
    if (ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) {
      check(node.moduleSpecifier);
    }
    if (ts.isCallExpression(node)) {
      const callee = node.expression;
      if (
        callee.kind === ts.SyntaxKind.ImportKeyword ||
        (ts.isIdentifier(callee) && REQUIRE_CALLEE.test(callee.text))
      ) {
        check(node.arguments[0]);
      }
    }
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
