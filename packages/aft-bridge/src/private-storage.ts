import {
  closeSync,
  constants,
  copyFileSync,
  fchmodSync,
  fstatSync,
  lstatSync,
  mkdirSync,
  openSync,
  readSync,
  writeSync,
} from "node:fs";
import { isAbsolute, join, parse, relative, resolve, sep } from "node:path";

export const PRIVATE_FILE_MODE = 0o600;
export const PRIVATE_DIRECTORY_MODE = 0o700;

/** Apply the mode to every newly created component, not after mkdir returns. */
export function privateMkdirSync(path: string, recursive = true): void {
  mkdirSync(path, {
    recursive,
    ...(process.platform === "win32" ? {} : { mode: PRIVATE_DIRECTORY_MODE }),
  });
}

/** copyFile copies the source mode too; private state must not inherit it. */
export function privateCopyFileSync(source: string, destination: string): void {
  if (process.platform === "win32") {
    copyFileSync(source, destination);
    return;
  }
  const input = openSync(source, "r");
  let output: number | undefined;
  try {
    output = openSync(destination, "wx", PRIVATE_FILE_MODE);
    const buffer = Buffer.allocUnsafe(64 * 1024);
    for (;;) {
      const bytes = readSync(input, buffer);
      if (bytes === 0) break;
      let offset = 0;
      while (offset < bytes) offset += writeSync(output, buffer, offset, bytes - offset);
    }
  } finally {
    closeSync(input);
    if (output !== undefined) closeSync(output);
  }
}

const warned = new Set<string>();

/** Only the opened storage ancestors are repaired; histories are never walked. */
export function openPrivateStorageDir(root: string, path = root): void {
  const base = resolve(root);
  if (!root || base === parse(base).root) {
    throw new Error(`refusing filesystem root as private storage: ${base}`);
  }
  const rel = relative(base, resolve(path));
  if (rel === ".." || rel.startsWith(`..${sep}`) || isAbsolute(rel)) return;
  privateMkdirSync(path);
  if (process.platform === "win32") return;
  let current = base;
  for (const component of ["", ...rel.split(sep).filter(Boolean)]) {
    current = join(current, component);
    let fd: number | undefined;
    try {
      if (lstatSync(current).isSymbolicLink()) return;
      fd = openSync(current, constants.O_RDONLY | constants.O_DIRECTORY | constants.O_NOFOLLOW);
      if ((fstatSync(fd).mode & 0o077) !== 0) fchmodSync(fd, PRIVATE_DIRECTORY_MODE);
    } catch (error) {
      if (!warned.has(current)) {
        warned.add(current);
        process.stderr.write(
          `[aft] could not tighten private storage ${current}: ${String(error)}; continuing\n`,
        );
      }
      return;
    } finally {
      if (fd !== undefined) closeSync(fd);
    }
  }
}
