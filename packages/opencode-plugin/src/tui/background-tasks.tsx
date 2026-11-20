import type { Dialog } from "@opencode/plugin/tui/context";
import type {
  AftBackgroundTask,
  AftBackgroundTaskSession,
  AftStopBackgroundTaskInput,
  AftStopBackgroundTaskResult,
} from "./aft-rpc";

export type BackgroundTasksDialog = Pick<Dialog, "alert" | "confirm" | "select">;

type LocationOptions = { location: unknown };

export type BackgroundTasksRpc = {
  getBackgroundTasks(
    input: AftBackgroundTaskSession,
    options?: LocationOptions,
  ): Promise<readonly AftBackgroundTask[]>;
  stopBackgroundTask(
    input: AftStopBackgroundTaskInput,
    options?: LocationOptions,
  ): Promise<AftStopBackgroundTaskResult>;
};

const TITLE = "AFT Background Tasks";
const STOP_MESSAGES = {
  stopped: "Background task stopped",
  finished: "Background task finished",
  "not-active": "Background task is no longer available to stop",
} satisfies Record<AftStopBackgroundTaskResult["outcome"], string>;

export async function showBackgroundTasksDialog(
  dialog: BackgroundTasksDialog,
  rpc: BackgroundTasksRpc,
  currentSessionID: () => string | undefined,
  location: unknown,
): Promise<void> {
  const sessionID = currentSessionID();
  if (!sessionID) {
    return dialog.alert({ title: TITLE, message: "Open a session to view its background tasks." });
  }

  const options = location ? { location } : undefined;
  const loadTasks = () => rpc.getBackgroundTasks({ sessionID }, options).catch(() => undefined);
  let tasks = await loadTasks();
  let stopMessage: string | undefined;

  // Each selection closes the native picker. Keep one snapshot for this flow,
  // and fetch again only on Refresh or after sending a Stop request.
  while (currentSessionID() === sessionID) {
    const selected = await dialog.select<AftBackgroundTask | "refresh">({
      title: stopMessage ?? TITLE,
      placeholder: "Filter tasks or select one to stop",
      options: [
        {
          title: "Refresh",
          value: "refresh",
          description: !tasks
            ? "Could not load background tasks"
            : tasks.length === 0
              ? "No active background tasks"
              : undefined,
        },
        ...(tasks ?? []).map((task) => ({
          value: task,
          title: task.command?.replace(/[\r\n]+/g, " ").trim() || `Task ${task.id}`,
          description: task.workdir ? `Working directory: ${task.workdir}` : undefined,
          footer:
            task.elapsedMs !== undefined
              ? `Elapsed: ${task.elapsedMs < 1000 ? `${task.elapsedMs} ms` : `${(task.elapsedMs / 1000).toFixed(1)} s`}`
              : undefined,
        })),
      ],
    });

    if (selected === undefined || currentSessionID() !== sessionID) return;
    stopMessage = undefined;
    if (selected !== "refresh") {
      const confirmed = await dialog.confirm({
        title: "Stop this background task?",
        message: selected.command
          ? `Command: ${selected.command}\nTask ID: ${selected.id}`
          : `Task ID: ${selected.id}`,
        label: { confirm: "Stop", cancel: "Cancel" },
      });
      if (!confirmed) continue;
      if (currentSessionID() !== sessionID) return;

      const result = await rpc
        .stopBackgroundTask({ sessionID, taskID: selected.id }, options)
        .catch(() => undefined);
      stopMessage = result ? STOP_MESSAGES[result.outcome] : "Could not confirm the task stopped";
    }

    tasks = await loadTasks();
  }
}
