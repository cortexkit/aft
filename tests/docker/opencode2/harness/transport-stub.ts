import { chmod, mkdir, rename, rm, symlink, writeFile } from "node:fs/promises";
import { isAbsolute, join, resolve } from "node:path";

export interface TransportDeadStub {
  /** The path handed to the plugin as `AFT_BINARY_PATH` for the whole run. */
  executable: string;
  /** The real binary that path points at while the transport is alive. */
  live: string;
  /** The stand-in it points at while the transport is dead. */
  dead: string;
  /** The script the stand-in's interpreter runs; its path never changes. */
  deadBody: string;
}

/**
 * What the stand-in a transport-dead window installs actually does.
 *
 * It reads the request the bridge writes to it and only then exits, without
 * answering. That order is the whole point: the request has provably been
 * handed over before the process dies, so nothing can say whether the command
 * ran, and bash has to refuse rather than run it a second time somewhere else.
 * The exit status is an arbitrary non-zero one, chosen so the line it leaves
 * in the plugin log is recognisably this stand-in rather than a real crash.
 */
export const DEAD_TRANSPORT_STUB_BODY = `# Accept the request the AFT bridge writes, then die without answering it.
read -r _request
exit 7
`;

/**
 * The file the swapped `aft` symlink points at while the transport is dead.
 *
 * It is only a `#!` line that names the body by its own fixed path. It must
 * not carry the body itself. When the kernel runs a `#!` script it reads the
 * `#!` line once, then starts the interpreter with the path it was ASKED to
 * run, and the interpreter opens that path again to read the script. Here
 * that path is the symlink the harness swaps. A bridge that respawned the
 * stand-in just before the window closed had its `/bin/sh` reopen the symlink
 * after it already pointed back at the real `aft`, and the shell ran the
 * native binary as a script in the project directory. The ELF header's first
 * "line" contains a `>` byte (the x86-64 machine id), so the shell redirected
 * into a new file named after the header bytes that follow it, and the
 * scenario's disk sweep then failed on that non-UTF-8 name. Whether the
 * header parses as a command line at all depends on the bytes of each build,
 * which is why one rebuilt binary failed every run and its predecessor did
 * not.
 *
 * Passing the body as the interpreter's own argument means the shell reads
 * the body from a path nothing ever swaps. The swapped path arrives only as
 * `$1`, which the body never reads. The whole `#!` line is one argument, so it
 * means the same on Linux, which passes the rest of the line as a single
 * argument, and on macOS, which splits it on whitespace.
 */
export function deadTransportStubSource(bodyPath: string): string {
  if (!isAbsolute(bodyPath) || /\s/.test(bodyPath)) {
    throw new Error(
      `the transport-dead stand-in body must be an absolute path without whitespace to fit in a #! line: ${JSON.stringify(bodyPath)}`,
    );
  }
  return `#!/bin/sh ${bodyPath}\n`;
}

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
  writeStandInFile?: StandInFileWriter,
): Promise<TransportDeadStub> {
  const stateRoot = resolve(isolationRoot, ".harness-state");
  await mkdir(stateRoot, { recursive: true });
  const write = writeStandInFile ?? isolationRootWriter(stateRoot);
  const deadBody = await write({
    name: "aft-dead-body.sh",
    content: DEAD_TRANSPORT_STUB_BODY,
    executable: false,
  });
  const dead = await write({
    name: "aft-dead",
    content: deadTransportStubSource(deadBody),
    executable: true,
  });
  const stub: TransportDeadStub = {
    executable: join(stateRoot, "aft"),
    live: nativeExecutable,
    dead,
    deadBody,
  };
  await pointTransportDeadStub(stub, false);
  return stub;
}

/**
 * Writes one of the stand-in's files and returns the absolute path it now
 * lives at. The swapped `aft` symlink always stays inside the isolation root;
 * only the files it points at can live elsewhere. Unit tests pass a writer
 * that reuses one fixed copy per content, because on macOS every newly
 * created executable is scanned by Gatekeeper the first time it runs.
 */
export type StandInFileWriter = (file: {
  name: string;
  content: string;
  executable: boolean;
}) => Promise<string>;

function isolationRootWriter(stateRoot: string): StandInFileWriter {
  return async ({ name, content, executable }) => {
    const path = join(stateRoot, name);
    await rm(path, { force: true });
    await writeFile(path, content);
    if (executable) await chmod(path, 0o755);
    return path;
  };
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
