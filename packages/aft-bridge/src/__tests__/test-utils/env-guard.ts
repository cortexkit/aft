type EnvOverrides = Record<string, string | undefined>;
type EnvSnapshot = Map<string, string | undefined>;

let envLockTail = Promise.resolve();

async function acquireEnvMutex(): Promise<() => void> {
  let releaseNext!: () => void;
  const waitForPrevious = envLockTail;
  envLockTail = new Promise<void>((resolve) => {
    releaseNext = resolve;
  });

  await waitForPrevious;

  let released = false;
  return () => {
    if (released) return;
    released = true;
    releaseNext();
  };
}

function snapshotEnv(overrides: EnvOverrides): EnvSnapshot {
  const snapshot: EnvSnapshot = new Map();
  for (const key of Object.keys(overrides)) {
    snapshot.set(key, process.env[key]);
  }
  return snapshot;
}

function applyEnv(values: EnvOverrides | EnvSnapshot): void {
  for (const [key, value] of values instanceof Map ? values : Object.entries(values)) {
    if (value === undefined) {
      delete process.env[key];
    } else {
      process.env[key] = value;
    }
  }
}

export async function withEnv<T>(overrides: EnvOverrides, fn: () => T | Promise<T>): Promise<T> {
  const release = await acquireEnvMutex();
  const snapshot = snapshotEnv(overrides);
  applyEnv(overrides);

  try {
    return await fn();
  } finally {
    applyEnv(snapshot);
    release();
  }
}

export async function acquireEnv(overrides: EnvOverrides): Promise<() => void> {
  const release = await acquireEnvMutex();
  const snapshot = snapshotEnv(overrides);
  applyEnv(overrides);

  return () => {
    applyEnv(snapshot);
    release();
  };
}
