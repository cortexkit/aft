/**
 * A recording of the host's own event stream for one scenario.
 *
 * The stream is opened as soon as the shared server is up and stays open for
 * the whole scenario, so an event that is created and answered in the same
 * millisecond is still recorded. Polling a list endpoint cannot see one of
 * those: by the time the poll lands the request is gone.
 */
export interface HostEvent {
  type: string;
  data: Record<string, unknown>;
}

function parseEventBlock(block: string): HostEvent | undefined {
  const payload = block
    .split("\n")
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice(5).trim())
    .join("");
  if (!payload) return undefined;
  try {
    const parsed = JSON.parse(payload) as { type?: unknown; data?: unknown };
    if (typeof parsed.type !== "string") return undefined;
    const data =
      parsed.data && typeof parsed.data === "object" && !Array.isArray(parsed.data)
        ? (parsed.data as Record<string, unknown>)
        : {};
    return { type: parsed.type, data };
  } catch {
    return undefined;
  }
}

export class HostEventRecorder {
  readonly events: HostEvent[] = [];
  #controller = new AbortController();
  #reading?: Promise<void>;
  /** Why the stream stopped early, when it did. */
  failure?: string;

  async start(endpoint: string, password: string): Promise<void> {
    const authorization = `Basic ${Buffer.from(`opencode:${password}`).toString("base64")}`;
    const response = await fetch(new URL("/api/event", endpoint), {
      headers: { authorization, accept: "text/event-stream" },
      signal: this.#controller.signal,
    });
    if (!response.ok || !response.body) {
      this.failure = `event stream returned ${response.status}`;
      return;
    }
    const body = response.body;
    this.#reading = (async () => {
      const decoder = new TextDecoder();
      let pending = "";
      try {
        for await (const chunk of body as unknown as AsyncIterable<Uint8Array>) {
          pending += decoder.decode(chunk, { stream: true });
          const blocks = pending.split("\n\n");
          pending = blocks.pop() ?? "";
          for (const block of blocks) {
            const event = parseEventBlock(block);
            if (event) this.events.push(event);
          }
        }
      } catch (error) {
        if (!this.#controller.signal.aborted) {
          this.failure = error instanceof Error ? error.message : String(error);
        }
      }
    })();
  }

  async stop(): Promise<void> {
    this.#controller.abort();
    await this.#reading?.catch(() => undefined);
  }

  ofType(type: string): HostEvent[] {
    return this.events.filter((event) => event.type === type);
  }

  /**
   * The id of the session the host created for this scenario.
   *
   * Taken from `session.created` on the stream rather than a list poll: the
   * stream is already open, so the id is known the moment the session exists,
   * with no request per attempt.
   */
  async awaitSessionId(timeoutMs: number): Promise<string | undefined> {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      for (const event of this.ofType("session.created")) {
        const id = event.data.sessionID ?? event.data.id;
        if (typeof id === "string" && id.length > 0) return id;
      }
      await Bun.sleep(25);
    }
    return undefined;
  }
}
