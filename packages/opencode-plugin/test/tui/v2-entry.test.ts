import { expect, test } from "bun:test";

import entry from "../../src/entry/tui.mjs";
import source from "../../src/tui/index.tsx";

test("the hybrid TUI entry wires V2 setup without changing the V1 initializer", () => {
  expect(entry.id).toBe("aft-opencode");
  expect(entry.tui).toBe(source.tui);
  expect(typeof source.setup).toBe("function");
  expect(typeof entry.setup).toBe("function");
});
