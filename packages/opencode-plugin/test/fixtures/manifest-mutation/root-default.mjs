const stateKey = Symbol.for("aft.load-matrix.manifest-mutation");
const state = (globalThis[stateKey] ??= {
  daemonOwners: new Set(),
  routes: new Map(),
  initCount: 0,
  disposeCount: 0,
});

export default async function initialize({ directory }) {
  state.daemonOwners.add("process-global");
  state.routes.set(directory, (state.routes.get(directory) ?? 0) + 1);
  state.initCount += 1;
  let disposed = false;

  return {
    dispose() {
      if (disposed) return;
      disposed = true;
      state.disposeCount += 1;
      const remaining = (state.routes.get(directory) ?? 1) - 1;
      if (remaining === 0) state.routes.delete(directory);
      else state.routes.set(directory, remaining);
    },
  };
}
