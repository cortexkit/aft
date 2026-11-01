// Load only the pinned prebuilt runtime and explicit grammar bytes. No package
// installation, grammar discovery, C compilation or Luau execution occurs.
const fs = require("node:fs");
const { Parser, Language } = require(process.argv[2]);

function snapshot(node, field = null) {
  const result = {
    type: node.type,
    field,
    named: node.isNamed,
    extra: node.isExtra,
    hasError: node.hasError,
    isMissing: node.isMissing,
    start: node.startIndex,
    end: node.endIndex,
    startPosition: node.startPosition,
    endPosition: node.endPosition,
  };
  if (node.childCount === 0) {
    result.text = node.text;
  } else {
    result.children = node.children.map((child, index) =>
      snapshot(child, node.fieldNameForChild(index)),
    );
  }
  return result;
}

async function main() {
  await Parser.init();
  const language = await Language.load(fs.readFileSync(process.argv[3]));
  if (language.abiVersion !== 15) throw new Error("expected Luau grammar ABI 15");
  const parser = new Parser();
  parser.setLanguage(language);
  const tree = parser.parse(fs.readFileSync(process.argv[4], "utf8"));
  if (!tree || tree.rootNode.hasError) throw new Error("Luau corpus failed to parse");
  process.stdout.write(`${JSON.stringify(snapshot(tree.rootNode), null, 2)}\n`);
  tree.delete();
  parser.delete();
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
