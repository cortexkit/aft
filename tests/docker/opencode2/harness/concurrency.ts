export const DEFAULT_E2E_CONCURRENCY = 4;

export function parseE2EConcurrency(value: string | undefined): number {
  if (value === undefined || value === "") return DEFAULT_E2E_CONCURRENCY;
  const concurrency = Number(value);
  if (!Number.isSafeInteger(concurrency) || concurrency < 1) {
    throw new Error(`AFT_E2E_CONCURRENCY must be a positive integer, got ${value}`);
  }
  return concurrency;
}

export async function mapWithConcurrency<T, R>(
  values: readonly T[],
  concurrency: number,
  worker: (value: T, index: number) => Promise<R>,
): Promise<R[]> {
  if (!Number.isSafeInteger(concurrency) || concurrency < 1) {
    throw new Error(`concurrency must be a positive integer, got ${concurrency}`);
  }
  const results = new Array<R>(values.length);
  let nextIndex = 0;
  const runWorker = async () => {
    for (;;) {
      const index = nextIndex;
      nextIndex += 1;
      if (index >= values.length) return;
      results[index] = await worker(values[index], index);
    }
  };
  await Promise.all(
    Array.from({ length: Math.min(concurrency, values.length) }, () => runWorker()),
  );
  return results;
}
