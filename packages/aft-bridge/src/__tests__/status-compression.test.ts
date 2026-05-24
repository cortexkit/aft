import { describe, expect, test } from "bun:test";
import type { StatusResponse } from "../protocol.js";

describe("status compression protocol", () => {
  test("status_response_typed_with_compression_aggregate", () => {
    const response: StatusResponse = {
      id: "status-1",
      success: true,
      compression: {
        project: {
          events: 3,
          original_tokens: 300,
          compressed_tokens: 210,
          savings_tokens: 90,
        },
        session: {
          events: 1,
          original_tokens: 100,
          compressed_tokens: 70,
          savings_tokens: 30,
        },
      },
    };

    expect(response.compression?.project.events).toBe(3);
    expect(response.compression?.session.savings_tokens).toBe(30);
  });

  test("status_response_compression_field_is_optional", () => {
    const response: StatusResponse = {
      id: "status-legacy",
      success: true,
      version: "0.26.4",
    };

    expect(response.compression).toBeUndefined();
  });
});
