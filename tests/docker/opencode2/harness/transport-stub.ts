import { chmod, mkdir, rename, rm, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";

export interface TransportDeadStub {
  /** The path handed to the plugin as `AFT_BINARY_PATH` for the whole run. */
  executable: string;
  /** The real binary that path points at while the transport is alive. */
  live: string;
  /** The stand-in it points at while the transport is dead. */
  dead: string;
}

/**
 * The stand-in binary a transport-dead window installs.
 *
 * It reads the request the bridge writes to it and only then exits, without
 * answering. That order is the whole point: the request has provably been
 * handed over before the process dies, so nothing can say whether the command
 * ran, and bash has to refuse rather than run it a second time somewhere else.
 * The exit status is an arbitrary non-zero one, chosen so the line it leaves
 * in the plugin log is recognisably this stand-in rather than a real crash.
 */
export const DEAD_TRANSPORT_STUB_SOURCE = `#!/bin/sh
# Accept the request the AFT bridge writes, then die without answering it.
read -r _request
exit 7
`;

/**
 * Build the switchable `aft` the transport-dead window points the plugin at.
 *
 * Two things have to be true at once, and a single file cannot do both.
 *
 * The plugin has to START: it registers its tools once, at plugin load, and
 * the bridge resolver refuses any `AFT_BINARY_PATH` that is not a native
 * executable — a deliberate guard against a `which aft` PATH lookup finding
 * the npm CLI's own `#!` shim and recursing into it. A shell stub installed
 * under that variable for the whole scenario threw during registration, and
 * the host ended up holding none of AFT's tools rather than a bash whose
 * transport dies mid-run. That resolution happens once, at load, so what the
 * path names afterwards is never re-checked.
 *
 * The dead state has to be a failure whose OUTCOME CANNOT BE DETERMINED,
 * because that is what these rows are about: bash refuses such a failure
 * rather than re-running the command through the host shell. A binary that is
 * simply missing is the opposite case — nothing can have been handed to a
 * process that never started — and the product deliberately treats it as
 * break-glass-eligible; the OpenCode plugin's own e2e suite asserts that the
 * host fallback RUNS when the bridge binary is missing. Pointing the dead
 * window at a missing path therefore made the outcome a race with the bridge's
 * own bookkeeping: while it still believed its killed child was alive it wrote
 * the request into that child and reported an outcome nobody can determine,
 * and once it had processed the death it respawned against the missing path
 * and reported a failure that proves nothing was sent. A CI run took the
 * second branch, the break-glass path opened, and the command ran for real.
 *
 * So the dead state is a stand-in that accepts the request and then dies. Both
 * orderings now end in an undeterminable outcome, and the refusal the rows
 * assert is a property of the transport failure rather than of the timing.
 *
 * The variable therefore names a symlink: the real binary while the plugin
 * starts and registers, the stand-in for the declared window.
 */
export async function makeTransportDeadStub(
  isolationRoot: string,
  nativeExecutable: string,
): Promise<TransportDeadStub> {
  const stateRoot = join(isolationRoot, ".harness-state");
  await mkdir(stateRoot, { recursive: true });
  const stub: TransportDeadStub = {
    executable: join(stateRoot, "aft"),
    live: nativeExecutable,
    dead: join(stateRoot, "aft-dead"),
  };
  await rm(stub.dead, { force: true });
  await writeFile(stub.dead, DEAD_TRANSPORT_STUB_SOURCE);
  await chmod(stub.dead, 0o755);
  await pointTransportDeadStub(stub, false);
  return stub;
}

/**
 * Point the scenario's `aft` at the real binary or at the stand-in.
 *
 * The swap is a rename over the symlink so anything about to spawn it sees one
 * state or the other, and so a process already running the old target keeps
 * running rather than dying halfway.
 */
export async function pointTransportDeadStub(
  stub: TransportDeadStub,
  dead: boolean,
): Promise<void> {
  const pending = `${stub.executable}.pending`;
  await rm(pending, { force: true });
  await symlink(dead ? stub.dead : stub.live, pending);
  await rename(pending, stub.executable);
}
