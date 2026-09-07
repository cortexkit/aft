import server from "../index.js";
import { serverEffect as effect } from "./server-runtime.mjs";

const id = "aft-opencode";

export default {
  id,
  server,
  effect,
};
