export interface WakeSessionApi {
  promptAsync(input: unknown): Promise<unknown> | unknown;
  message?: (input: {
    path: { id: string; messageID: string };
    signal?: AbortSignal;
    throwOnError?: false;
    responseStyle?: "fields";
  }) => Promise<unknown> | unknown;
  messages?: (input: {
    path: { id: string };
    query?: { limit?: number };
    signal?: AbortSignal;
    throwOnError?: false;
    responseStyle?: "fields";
  }) => Promise<unknown> | unknown;
}

export interface PendingWakeAdmission {
  messageID: string;
  taskIDs: readonly string[];
  status: "submitting" | "unknown" | "pending";
  observe(info: Record<string, unknown>): void;
  reconcile(): Promise<void>;
  cancel(): void;
}

export class WakeAdmissionRejected extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

interface AdmissionOptions {
  deliveryID: string;
  sessionID: string;
  requestBody: Record<string, unknown> & { messageID: string };
  sessionApi: WakeSessionApi;
  pending: Map<string, PendingWakeAdmission>;
  timeoutMs: number;
  onRecovery: () => void;
  onPending: () => void;
  onLateAcceptance: () => void;
  taskIDs: readonly string[];
  onUnknown: (cause: unknown) => void;
}

interface AdmissionResult {
  accepted: boolean;
  pending?: boolean;
  ambiguous: boolean;
  turnObserved: boolean;
}

const READBACK_INTERVAL_MS = 2_000;
const ADMISSION_RECOVERY_WINDOW_MS = 120_000;

function record(value: unknown): Record<string, unknown> | undefined {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

function responseFailure(response: unknown): string | undefined {
  const value = record(response);
  if (value?.error != null)
    return typeof value.error === "string" ? value.error : JSON.stringify(value.error);
  if (value?.success === false)
    return String(value.message ?? "promptAsync returned success=false");
  return undefined;
}

/** A recorded user message must contain the submitted model-visible text, not just its id. */
function containsSubmittedText(
  value: unknown,
  sessionID: string,
  body: Record<string, unknown>,
): boolean {
  const message = record(value);
  const info = record(message?.info);
  if (info?.role !== "user" || info.id !== body.messageID || info.sessionID !== sessionID)
    return false;
  const expected = Array.isArray(body.parts)
    ? body.parts.map(record).filter((part) => part?.type === "text")
    : [];
  const parts = Array.isArray(message?.parts) ? message.parts.map(record) : [];
  return (
    expected.length > 0 &&
    expected.every((part) =>
      parts.some(
        (actual) =>
          actual?.type === "text" &&
          actual.text === part?.text &&
          actual.synthetic === true &&
          actual.ignored !== true,
      ),
    )
  );
}

/** Bound readback independently; a missing or failed read is never proof of rejection. */
async function readHostMessage(
  options: AdmissionOptions,
  controller: AbortController,
): Promise<unknown> {
  const { sessionApi, sessionID, requestBody, timeoutMs } = options;
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    const read =
      typeof sessionApi.message === "function"
        ? Promise.resolve().then(() =>
            sessionApi.message?.({
              path: { id: sessionID, messageID: requestBody.messageID },
              signal: controller.signal,
              throwOnError: false,
              responseStyle: "fields",
            }),
          )
        : typeof sessionApi.messages === "function"
          ? Promise.resolve().then(() =>
              sessionApi.messages?.({
                path: { id: sessionID },
                query: { limit: 50 },
                signal: controller.signal,
                throwOnError: false,
                responseStyle: "fields",
              }),
            )
          : Promise.resolve(undefined);
    return await Promise.race([
      read,
      new Promise<undefined>((resolve) => {
        timer = setTimeout(() => {
          controller.abort();
          resolve(undefined);
        }, timeoutMs);
        timer.unref?.();
      }),
    ]);
  } catch {
    return undefined;
  } finally {
    if (timer) clearTimeout(timer);
    controller.abort();
  }
}

/**
 * The V1 SDK's prompt_async HTTP 204 accepts background prompt work before
 * the assistant finishes it. For an uncertain response, check the submitted
 * message's text or assistant child for two minutes. One recovery then reuses
 * the original messageID and body; that identity is not a deduplication promise.
 * Each request has its own timeout budget. If recovery is also unproven, keep
 * the pending wake reminders for the next tool reply/model turn, without a third automatic
 * submission for this ambiguous delivery.
 */
export async function awaitWakeAdmission(options: AdmissionOptions): Promise<AdmissionResult> {
  let requestController = new AbortController();
  let requestTimer: ReturnType<typeof setTimeout> | undefined;
  let recoveryTimer: ReturnType<typeof setTimeout> | undefined;
  let pollTimer: ReturnType<typeof setTimeout> | undefined;
  let readController: AbortController | undefined;
  let readInFlight: Promise<void> | undefined;
  let readAgain = false;
  let ambiguous = false;
  let recovered = false;
  let completed = false;
  let returnedPending = false;
  let finish!: (result: AdmissionResult) => void;
  let reject!: (error: Error) => void;
  const result = new Promise<AdmissionResult>((resolve, fail) => {
    finish = resolve;
    reject = fail;
  });
  const current = () => !completed && options.pending.get(options.deliveryID) === admission;
  const clearTimers = () => {
    if (requestTimer) clearTimeout(requestTimer);
    if (recoveryTimer) clearTimeout(recoveryTimer);
    if (pollTimer) clearTimeout(pollTimer);
    requestTimer = recoveryTimer = pollTimer = undefined;
  };
  const detach = () => {
    clearTimers();
    requestController.abort();
    readController?.abort();
    if (options.pending.get(options.deliveryID) === admission)
      options.pending.delete(options.deliveryID);
  };
  const accept = (turnObserved = false) => {
    if (!current()) return;
    completed = true;
    detach();
    if (returnedPending) options.onLateAcceptance();
    else finish({ accepted: true, ambiguous, turnObserved });
  };
  const leavePending = () => {
    if (!current() || returnedPending) return;
    returnedPending = true;
    admission.status = "pending";
    clearTimers();
    requestController.abort();
    readController?.abort();
    // Restore queues before resolving, so a host event cannot race their restoration.
    options.onPending();
    finish({ accepted: false, pending: true, ambiguous, turnObserved: false });
  };
  const admission: PendingWakeAdmission = {
    messageID: String(options.requestBody.messageID),
    taskIDs: options.taskIDs,
    status: "submitting",
    observe(info) {
      if (info.sessionID !== options.sessionID) return;
      if (info.role === "assistant" && info.parentID === admission.messageID) accept(true);
      else if (info.role === "user" && info.id === admission.messageID) void admission.reconcile();
    },
    reconcile() {
      if (!current() || !ambiguous) return Promise.resolve();
      if (readInFlight) {
        readAgain = true;
        return readInFlight;
      }
      readController = new AbortController();
      readInFlight = readHostMessage(options, readController)
        .then((response) => {
          const data = record(response)?.data ?? response;
          const messages = Array.isArray(data) ? data : [data];
          if (
            messages.some((message) =>
              containsSubmittedText(message, options.sessionID, options.requestBody),
            )
          )
            accept();
        })
        .catch(() => undefined)
        .finally(() => {
          readInFlight = undefined;
          if (readAgain && current()) {
            readAgain = false;
            void admission.reconcile();
          }
        });
      return readInFlight;
    },
    cancel() {
      if (!current()) return;
      completed = true;
      detach();
      if (!returnedPending) finish({ accepted: false, ambiguous, turnObserved: false });
    },
  };
  options.pending.set(options.deliveryID, admission);
  const poll = () => {
    if (!current() || returnedPending) return;
    void admission.reconcile();
    pollTimer = setTimeout(poll, READBACK_INTERVAL_MS);
    pollTimer.unref?.();
  };
  const unknown = (cause: unknown) => {
    if (!current()) return;
    if (!ambiguous) {
      ambiguous = true;
      admission.status = "unknown";
      if (requestTimer) clearTimeout(requestTimer);
      requestController.abort();
      options.onUnknown(cause);
      poll();
      recoveryTimer = setTimeout(() => {
        recoveryTimer = undefined;
        if (!current() || recovered) return;
        // Set the guard before invoking callbacks or dispatching the recovery request.
        recovered = true;
        options.onRecovery();
        submit();
      }, ADMISSION_RECOVERY_WINDOW_MS);
      recoveryTimer.unref?.();
    } else if (recovered && !returnedPending) {
      leavePending();
    }
  };
  const submit = () => {
    if (!current()) return;
    requestController = new AbortController();
    const controller = requestController;
    if (requestTimer) clearTimeout(requestTimer);
    requestTimer = setTimeout(() => {
      controller.abort();
      unknown(new Error(`session promptAsync injection timed out after ${options.timeoutMs}ms`));
    }, options.timeoutMs);
    requestTimer.unref?.();
    void Promise.resolve()
      .then(() => {
        if (!current() || controller.signal.aborted) return undefined;
        return options.sessionApi.promptAsync({
          path: { id: options.sessionID },
          body: options.requestBody,
          signal: controller.signal,
          throwOnError: false,
          responseStyle: "fields",
        });
      })
      .then(
        (response) => {
          if (!current()) return;
          const failure = responseFailure(response);
          const status = record(record(response)?.response)?.status;
          const error = record(record(response)?.error);
          // These SDK error responses come from request validation or a missing
          // session, which the server checks before scheduling background prompt work.
          const refused =
            (status === 400 && error?.name === "BadRequest") ||
            (status === 404 && error?.name === "NotFoundError");
          if (refused && !recovered && controller === requestController) {
            completed = true;
            detach();
            reject(
              new WakeAdmissionRejected(
                failure ?? `promptAsync rejected (${status})`,
                status as number,
              ),
            );
          } else if (status === 204 || (status === undefined && !failure)) {
            accept();
          } else {
            // A late rejection of recovery does not prove the original was rejected.
            if (controller === requestController)
              unknown(new Error(failure ?? `Unexpected promptAsync status ${status}`));
          }
        },
        (cause) => {
          if (controller !== requestController) return;
          unknown(cause);
        },
      )
      .catch(unknown);
  };
  submit();
  return result;
}
