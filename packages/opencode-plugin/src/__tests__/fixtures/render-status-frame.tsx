/** @jsxImportSource @opentui/solid */
import { testRender } from "@opentui/solid";
import { coerceAftStatus } from "../../shared/status.js";
import { StatusDialog } from "../../tui/index.tsx";

const palette = {
  text: "#ffffff",
  textMuted: "#888888",
  success: "#00ff00",
  warning: "#ffff00",
  error: "#ff0000",
  accent: "#0088ff",
  background: "#000000",
  border: "#444444",
};
const status = coerceAftStatus({
  cache_role: "main",
  checkpoints_total: null,
  session: { checkpoints: null },
});
const setup = await testRender(
  () => (
    <StatusDialog
      api={{ theme: { current: palette } } as never}
      client={{} as never}
      directory="/project"
      sessionID="session"
      initial={status}
      initialError={null}
    />
  ),
  { width: 100, height: 100 },
);
try {
  await setup.renderOnce();
  process.stdout.write(setup.captureCharFrame());
} finally {
  setup.renderer.destroy();
}
