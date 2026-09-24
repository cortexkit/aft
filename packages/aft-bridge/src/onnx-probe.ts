/**
 * Cheap loadability check for a system ONNX Runtime library, without loading it.
 *
 * A system install can carry a compatible version number and still be
 * unloadable. The motivating case is a Homebrew `libonnxruntime.dylib` linked
 * against an abseil release that is no longer installed: `dlopen` fails with
 * "Library not loaded: /opt/homebrew/opt/abseil/lib/libabsl_...dylib", yet the
 * file name says 1.2x and the resolver used to prefer it over AFT's own
 * downloaded runtime, leaving semantic search dead.
 *
 * Loading the library to find out is not an option in the plugin process: a
 * failed or half-successful `dlopen` of a native library runs its static
 * initializers inside the host (OpenCode, Pi) and cannot be undone. Instead
 * this reads the Mach-O load commands and checks that every absolute-path
 * dependency the library requires exists on disk. That is exactly the check
 * dyld fails on in the case above, and it costs one bounded file read.
 *
 * Scope, deliberately narrow:
 *   - Mach-O (macOS) only. ELF `DT_NEEDED` entries are bare sonames resolved
 *     through ld.so's cache and search path, and PE imports go through the
 *     Windows loader's search order; neither can be checked faithfully by
 *     reading the file, so those formats report `checked: false`.
 *   - Direct dependencies only; a missing transitive dependency still fails at
 *     load time in the child, where the Rust side reports it.
 *   - `@rpath`/`@loader_path`/`@executable_path` entries are skipped because
 *     resolving them needs the loading image's rpaths. System paths
 *     (`/usr/lib`, `/System`) are skipped because macOS serves them from the
 *     dyld shared cache and they do not exist as files.
 *   - Weak dependencies (`LC_LOAD_WEAK_DYLIB`) are skipped: dyld tolerates
 *     their absence.
 */

import { closeSync, existsSync, openSync, readSync, realpathSync } from "node:fs";

export type OnnxRuntimeLoadProbe =
  | { loadable: true; checked: boolean }
  | { loadable: false; reason: string };

const MH_MAGIC = 0xfeedface;
const MH_MAGIC_64 = 0xfeedfacf;
const FAT_MAGIC = 0xcafebabe;
const LC_LOAD_DYLIB = 0xc;
const LC_REEXPORT_DYLIB = 0x8000001f;
const LC_LOAD_UPWARD_DYLIB = 0x80000023;
const CPU_TYPE_X86_64 = 0x01000007;
const CPU_TYPE_ARM64 = 0x0100000c;
/** Load commands of real runtimes are a few KB; anything past this is not worth parsing. */
const MAX_HEADER_BYTES = 4 * 1024 * 1024;

const UNCHECKED: OnnxRuntimeLoadProbe = { loadable: true, checked: false };

function readAt(fd: number, offset: number, length: number): Buffer {
  const buffer = Buffer.alloc(length);
  const read = readSync(fd, buffer, 0, length, offset);
  return buffer.subarray(0, read);
}

function cpuTypeForArch(arch: string): number | null {
  if (arch === "arm64") return CPU_TYPE_ARM64;
  if (arch === "x64") return CPU_TYPE_X86_64;
  return null;
}

function cpuTypeName(cpuType: number): string {
  if (cpuType === CPU_TYPE_ARM64) return "arm64";
  if (cpuType === CPU_TYPE_X86_64) return "x86_64";
  return `cputype 0x${cpuType.toString(16)}`;
}

/** Offset of the Mach-O image for `arch` inside a universal file, or a reason it has none. */
function selectFatSlice(fd: number, arch: string): number | string {
  const header = readAt(fd, 0, 8);
  const count = header.readUInt32BE(4);
  const wanted = cpuTypeForArch(arch);
  const table = readAt(fd, 8, Math.min(count, 64) * 20);
  const found: string[] = [];
  for (let i = 0; i + 20 <= table.length; i += 20) {
    const cpuType = table.readUInt32BE(i);
    found.push(cpuTypeName(cpuType));
    if (wanted === null || cpuType === wanted) return table.readUInt32BE(i + 8);
  }
  return `universal library has no ${arch} slice (found ${found.join(", ") || "none"})`;
}

function requiredDylibs(fd: number, imageOffset: number, arch: string): string[] | string | null {
  const head = readAt(fd, imageOffset, 32);
  if (head.length < 28) return null;
  const magic = head.readUInt32LE(0);
  if (magic !== MH_MAGIC && magic !== MH_MAGIC_64) return null;
  const cpuType = head.readUInt32LE(4);
  const wanted = cpuTypeForArch(arch);
  if (wanted !== null && cpuType !== wanted) {
    return `built for ${cpuTypeName(cpuType)}, but AFT runs as ${arch}`;
  }
  const commandCount = head.readUInt32LE(16);
  const commandBytes = head.readUInt32LE(20);
  if (commandBytes > MAX_HEADER_BYTES) return null;
  const headerSize = magic === MH_MAGIC_64 ? 32 : 28;
  const commands = readAt(fd, imageOffset + headerSize, commandBytes);

  const dylibs: string[] = [];
  let cursor = 0;
  for (let i = 0; i < commandCount && cursor + 8 <= commands.length; i += 1) {
    const cmd = commands.readUInt32LE(cursor);
    const size = commands.readUInt32LE(cursor + 4);
    if (size < 8 || cursor + size > commands.length) break;
    if (cmd === LC_LOAD_DYLIB || cmd === LC_REEXPORT_DYLIB || cmd === LC_LOAD_UPWARD_DYLIB) {
      const nameOffset = commands.readUInt32LE(cursor + 8);
      if (nameOffset < size) {
        const raw = commands.subarray(cursor + nameOffset, cursor + size);
        const end = raw.indexOf(0);
        dylibs.push(raw.subarray(0, end === -1 ? raw.length : end).toString("utf8"));
      }
    }
    cursor += size;
  }
  return dylibs;
}

function isUncheckableDependency(path: string): boolean {
  return path.startsWith("@") || path.startsWith("/usr/lib/") || path.startsWith("/System/");
}

/**
 * Report whether the library at `libPath` can plausibly be loaded by an AFT
 * binary running as `arch` (defaults to this process's arch). Never throws.
 */
export function probeOnnxRuntimeLoadable(
  libPath: string,
  arch: string = process.arch,
): OnnxRuntimeLoadProbe {
  let fd: number;
  try {
    fd = openSync(realpathSync(libPath), "r");
  } catch (err) {
    return {
      loadable: false,
      reason: `cannot open library: ${err instanceof Error ? err.message : String(err)}`,
    };
  }
  try {
    const magic = readAt(fd, 0, 4);
    if (magic.length < 4) return { loadable: false, reason: "library file is truncated" };
    let imageOffset = 0;
    if (magic.readUInt32BE(0) === FAT_MAGIC) {
      const slice = selectFatSlice(fd, arch);
      if (typeof slice === "string") return { loadable: false, reason: slice };
      imageOffset = slice;
    }
    const dylibs = requiredDylibs(fd, imageOffset, arch);
    if (dylibs === null) return UNCHECKED;
    if (typeof dylibs === "string") return { loadable: false, reason: dylibs };
    for (const dependency of dylibs) {
      if (isUncheckableDependency(dependency)) continue;
      if (!existsSync(dependency)) {
        return { loadable: false, reason: `missing dependency ${dependency}` };
      }
    }
    return { loadable: true, checked: true };
  } catch {
    // A malformed header is not evidence the loader would reject the file;
    // leave the decision to the version checks and the Rust-side load.
    return UNCHECKED;
  } finally {
    closeSync(fd);
  }
}
