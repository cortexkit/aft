export class Runtime {
  publish(receipt: string, rawPatch: string): string {
    return `runtime:${receipt}:${rawPatch}`;
  }
}

export class SnapshotStore {
  publish(receipt: string, rawPatch: string): string {
    return `snapshot:${receipt}:${rawPatch}`;
  }
}
