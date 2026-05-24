/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { getActiveLogger, log, setActiveLogger } from "../active-logger.js";
import type { Logger } from "../logger.js";

// Active logger lives on a process-global Symbol slot. Tests that install a
// custom logger MUST restore the previous logger in afterEach, otherwise the
// no-op or throwing logger leaks into sibling test files via bun's shared
// process. Earlier this leaked into resolver-version-mismatch.test.ts on
// Linux CI, silently swallowing diagnostic `warn(...)` calls and changing
// the resolver's apparent behavior.
describe("active logger", () => {
  let prevLogger: Logger | undefined;

  beforeEach(() => {
    prevLogger = getActiveLogger();
  });

  afterEach(() => {
    if (prevLogger) setActiveLogger(prevLogger);
  });

  test("stores logger on Symbol.for global slot", () => {
    const logger: Logger = {
      log: () => undefined,
      warn: () => undefined,
      error: () => undefined,
    };

    setActiveLogger(logger);

    expect(getActiveLogger()).toBe(logger);
    expect((globalThis as Record<symbol, unknown>)[Symbol.for("aft-bridge-active-logger")]).toBe(
      logger,
    );
  });

  test("logger exceptions are caught and do not escape", () => {
    const logger: Logger = {
      log: () => {
        throw new Error("logger exploded");
      },
      warn: () => undefined,
      error: () => undefined,
    };
    setActiveLogger(logger);

    expect(() => log("still safe")).not.toThrow();
  });
});
