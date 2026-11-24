import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import {
  BashOutputPageDecoder,
  REGEX_WINDOW_SCAN_INCOMPLETE_TEXT,
  TERMINAL_SCAN_INCOMPLETE_TEXT,
} from "../bash-output-page.js";

describe("raw output page decoding", () => {
  test("approved decoder retains a split UTF-8 prefix with its byte offset", () => {
    const decoder = new BashOutputPageDecoder();
    const bytes = Buffer.from("ok € done");
    expect(decoder.decode(bytes.subarray(0, 4), 0, false)).toEqual({ text: "ok ", baseOffset: 0 });
    expect(decoder.decode(bytes.subarray(4), 4, true)).toEqual({ text: "€ done", baseOffset: 3 });
  });

  test("all UTF-8 split positions preserve complete text", () => {
    const text = "a€😀bÿc";
    const bytes = Buffer.from(text);
    for (let split = 1; split < bytes.length; split++) {
      const decoder = new BashOutputPageDecoder();
      const first = decoder.decode(bytes.subarray(0, split), 0, false);
      const second = decoder.decode(bytes.subarray(split), split, true);
      expect(first.text + second.text).toBe(text);
      expect(second.baseOffset).toBe(Buffer.byteLength(first.text));
    }
  });

  test("terminal incomplete and invalid sequences stay lossy instead of disappearing", () => {
    const decoder = new BashOutputPageDecoder();
    expect(decoder.decode(Buffer.from([0xe2]), 0, false).text).toBe("");
    expect(decoder.decode(Buffer.alloc(0), 1, true)).toEqual({ text: "�", baseOffset: 0 });
    const invalid = Buffer.from([0xe0, 0x80, 0x80, 0xff]);
    expect(new BashOutputPageDecoder().decode(invalid, 0, true).text).toBe(
      invalid.toString("utf8"),
    );
  });

  test("a discontinuous page cannot complete a previous stream's character", () => {
    const decoder = new BashOutputPageDecoder();
    decoder.decode(Buffer.from([0xe2]), 0, false);
    expect(decoder.decode(Buffer.from("new"), 10, true)).toEqual({ text: "new", baseOffset: 10 });
  });
});

describe("watch sentences shared with the engine", () => {
  // The engine's own bash_watch renders these sentences too. Reading the Rust
  // source keeps the hosts and the engine from drifting apart.
  const engineWatch = readFileSync(
    new URL("../../../../crates/aft-tools/src/commands/bash_watch.rs", import.meta.url),
    "utf8",
  );

  test("the window-drop sentence is the engine's REGEX_WINDOW_SCAN_INCOMPLETE", () => {
    const found = /const REGEX_WINDOW_SCAN_INCOMPLETE: &str = "([^"]*)";/.exec(engineWatch);
    expect(found?.[1]).toBe(REGEX_WINDOW_SCAN_INCOMPLETE_TEXT);
  });

  test("the mid-drain timeout sentence is the engine's", () => {
    expect(engineWatch).toContain(`{waited_text}; ${TERMINAL_SCAN_INCOMPLETE_TEXT}`);
  });
});
