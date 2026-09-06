import legacy from "../tui/entry.mjs";

const setup = (...args) => legacy.setup(...args);

export default {
  id: legacy.id,
  tui: legacy.tui,
  setup,
};
