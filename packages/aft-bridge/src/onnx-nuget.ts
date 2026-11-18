import { type FileHandle, open } from "node:fs/promises";
import { createInflateRaw } from "node:zlib";
import { PRIVATE_FILE_MODE } from "./private-storage.js";

const MAX_PACKAGE_BYTES = 200 * 1024 * 1024;
const MAX_DIRECTORY_BYTES = 4 * 1024 * 1024;
const MAX_COMPRESSED_BYTES = 64 * 1024 * 1024;
const MAX_LIBRARY_BYTES = 128 * 1024 * 1024;
const END_SIZE = 22;
const ZIP64_LOCATOR_SIZE = 20;
const ZIP_TAIL_BYTES = END_SIZE + 0xffff + ZIP64_LOCATOR_SIZE;
const CENTRAL_HEADER_SIZE = 46;
const LOCAL_HEADER_SIZE = 30;

async function readBody(
  response: Response,
  cap: number,
  expected: number | undefined,
  consume: (chunk: Uint8Array, offset: number) => void | Promise<void>,
): Promise<number> {
  const reader = response.body?.getReader();
  if (!reader) throw new Error("download response has no body");
  let size = 0;
  let finished = false;
  try {
    if (expected !== undefined && expected > cap) {
      throw new Error(`download exceeds ${cap} bytes`);
    }
    const lengthHeader = response.headers.get("content-length");
    if (lengthHeader !== null) {
      const length = Number(lengthHeader);
      if (!/^\d+$/.test(lengthHeader) || !Number.isSafeInteger(length) || length > cap) {
        throw new Error(`download exceeds ${cap} bytes or has invalid Content-Length`);
      }
      if (expected !== undefined && length !== expected) {
        throw new Error(`incorrect download Content-Length: expected ${expected}, got ${length}`);
      }
      expected = length;
    }
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      const next = size + value.byteLength;
      if (next > (expected ?? cap)) throw new Error("download body exceeds its byte limit");
      await consume(value, size);
      size = next;
    }
    if (expected !== undefined && size !== expected) {
      throw new Error(`short download body: expected ${expected} bytes, got ${size}`);
    }
    finished = true;
    return size;
  } finally {
    if (!finished) await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}

function rangeBounds(start: number | null, count: number, total: number) {
  const first = start ?? Math.max(0, total - count);
  const last = start === null ? total - 1 : first + count - 1;
  if (first > last || last >= total) throw new Error("ZIP range lies outside the package");
  return { first, last, size: last - first + 1 };
}

async function readFileRange(file: FileHandle, first: number, size: number): Promise<Buffer> {
  const bytes = Buffer.alloc(size);
  let offset = 0;
  while (offset < size) {
    const { bytesRead } = await file.read(bytes, offset, size - offset, first + offset);
    if (!bytesRead) throw new Error("short ZIP file read");
    offset += bytesRead;
  }
  return bytes;
}

/** Read one named native library without extracting any other NuGet package entries. */
export async function readOnnxNugetLibrary(
  url: string,
  entryName: string,
  fallbackPath: string,
): Promise<Buffer> {
  let total: number | undefined;
  let fullPackage: FileHandle | undefined;
  const signal = AbortSignal.timeout(3 * 60 * 1000);

  async function readRange(start: number | null, count: number): Promise<Buffer> {
    if (!fullPackage) {
      const range = start === null ? `bytes=-${count}` : `bytes=${start}-${start + count - 1}`;
      const response = await fetch(url, {
        headers: { Range: range, "Accept-Encoding": "identity" },
        signal,
      });
      try {
        const encoding = response.headers.get("content-encoding");
        if (encoding && encoding.toLowerCase() !== "identity") {
          throw new Error(`unexpected download Content-Encoding: ${encoding}`);
        }
        if (response.status === 206) {
          const contentRange = /^bytes (\d+)-(\d+)\/(\d+)$/.exec(
            response.headers.get("content-range") ?? "",
          );
          const length = Number(contentRange?.[3]);
          if (!Number.isSafeInteger(length) || length <= 0 || length >= 0xffffffff) {
            throw new Error("invalid download Content-Range or ZIP64 package");
          }
          const { first, last, size } = rangeBounds(start, count, length);
          if (
            Number(contentRange?.[1]) !== first ||
            Number(contentRange?.[2]) !== last ||
            (total !== undefined && total !== length)
          ) {
            throw new Error("download Content-Range does not match requested ZIP bytes");
          }
          total = length;
          const bytes = Buffer.alloc(size);
          await readBody(response, size, size, (chunk, offset) => {
            bytes.set(chunk, offset);
          });
          return bytes;
        }

        if (response.status !== 200) {
          throw new Error(`download failed (HTTP ${response.status})`);
        }
        // Servers that ignore Range may send the entire package. Spool it to disk
        // so the fallback does not keep all platforms' libraries in memory.
        fullPackage = await open(fallbackPath, "wx+", PRIVATE_FILE_MODE);
        const file = fullPackage;
        const size = await readBody(response, MAX_PACKAGE_BYTES, total, (chunk) =>
          file.writeFile(chunk),
        );
        total = size;
      } catch (cause) {
        await response.body?.cancel().catch(() => {});
        throw cause;
      }
    }

    if (total === undefined) throw new Error("missing ZIP package size");
    const { first, size } = rangeBounds(start, count, total);
    return readFileRange(fullPackage, first, size);
  }

  try {
    const tail = await readRange(null, ZIP_TAIL_BYTES);
    if (total === undefined) throw new Error("missing ZIP package size");
    const directory = locateDirectory(tail, total);
    const central = await readRange(directory.offset, directory.size);
    const entry = locateEntry(central, directory.entries, Buffer.from(entryName));
    return await readEntry(readRange, entry, directory.offset);
  } finally {
    await fullPackage?.close();
  }
}

function locateDirectory(tail: Buffer, total: number) {
  for (let end = tail.length - END_SIZE; end >= 0; end--) {
    if (
      tail.readUInt32LE(end) !== 0x06054b50 ||
      tail.readUInt16LE(end + 20) !== tail.length - end - END_SIZE
    ) {
      continue;
    }
    const disk = tail.readUInt16LE(end + 4);
    const directoryDisk = tail.readUInt16LE(end + 6);
    const diskEntries = tail.readUInt16LE(end + 8);
    const entries = tail.readUInt16LE(end + 10);
    const size = tail.readUInt32LE(end + 12);
    const offset = tail.readUInt32LE(end + 16);
    if (
      (end >= ZIP64_LOCATOR_SIZE && tail.readUInt32LE(end - ZIP64_LOCATOR_SIZE) === 0x07064b50) ||
      entries === 0xffff ||
      size === 0xffffffff ||
      offset === 0xffffffff
    ) {
      throw new Error("ZIP64 packages are not supported");
    }
    if (disk !== 0 || directoryDisk !== 0 || diskEntries !== entries) {
      throw new Error("multi-disk ZIP packages are not supported");
    }
    if (
      entries === 0 ||
      size < entries * CENTRAL_HEADER_SIZE ||
      size > MAX_DIRECTORY_BYTES ||
      offset + size !== total - tail.length + end
    ) {
      throw new Error("invalid ZIP central directory bounds");
    }
    return { entries, size, offset };
  }
  throw new Error("ZIP end of central directory not found");
}

function checkEntryFields(extra: Buffer, flags: number, ...values: number[]): void {
  if (flags & 0x2041) throw new Error("encrypted ZIP entries are not supported");
  if (values.includes(0xffffffff)) throw new Error("ZIP64 entries are not supported");
  for (let offset = 0; offset < extra.length; ) {
    if (offset + 4 > extra.length) throw new Error("invalid ZIP extra field");
    if (extra.readUInt16LE(offset) === 1) throw new Error("ZIP64 entries are not supported");
    offset += 4 + extra.readUInt16LE(offset + 2);
    if (offset > extra.length) throw new Error("invalid ZIP extra field");
  }
}

interface ZipEntry {
  name: Buffer;
  flags: number;
  method: number;
  compressedSize: number;
  size: number;
  offset: number;
}

function locateEntry(central: Buffer, entries: number, name: Buffer): ZipEntry {
  let match: ZipEntry | undefined;
  let cursor = 0;
  for (let index = 0; index < entries; index++) {
    const headerEnd = cursor + CENTRAL_HEADER_SIZE;
    if (headerEnd > central.length || central.readUInt32LE(cursor) !== 0x02014b50) {
      throw new Error("invalid ZIP central directory header");
    }
    const flags = central.readUInt16LE(cursor + 8);
    const method = central.readUInt16LE(cursor + 10);
    const compressedSize = central.readUInt32LE(cursor + 20);
    const size = central.readUInt32LE(cursor + 24);
    const nameEnd = headerEnd + central.readUInt16LE(cursor + 28);
    const extraEnd = nameEnd + central.readUInt16LE(cursor + 30);
    const next = extraEnd + central.readUInt16LE(cursor + 32);
    const disk = central.readUInt16LE(cursor + 34);
    const offset = central.readUInt32LE(cursor + 42);
    if (next > central.length || disk !== 0) throw new Error("invalid ZIP central directory entry");
    checkEntryFields(central.subarray(nameEnd, extraEnd), flags, size, compressedSize, offset);
    if (central.subarray(headerEnd, nameEnd).equals(name)) {
      if (match) throw new Error(`duplicate ZIP entry: ${name.toString()}`);
      match = { name, flags, method, compressedSize, size, offset };
    }
    cursor = next;
  }
  if (cursor !== central.length) throw new Error("invalid ZIP central directory size");
  if (!match) throw new Error(`missing ZIP entry: ${name.toString()}`);
  return match;
}

async function readEntry(
  readRange: (start: number, count: number) => Promise<Buffer>,
  entry: ZipEntry,
  directoryOffset: number,
): Promise<Buffer> {
  const { name, flags, method, compressedSize, size, offset } = entry;
  if (method !== 0 && method !== 8) throw new Error(`unsupported ZIP compression method ${method}`);
  if (
    compressedSize === 0 ||
    compressedSize > MAX_COMPRESSED_BYTES ||
    size === 0 ||
    size > MAX_LIBRARY_BYTES ||
    (method === 0 && size !== compressedSize)
  ) {
    throw new Error("invalid ZIP library size");
  }
  const headerEnd = offset + LOCAL_HEADER_SIZE;
  if (headerEnd + name.length + compressedSize > directoryOffset) {
    throw new Error("ZIP library overlaps the central directory");
  }
  const header = await readRange(offset, LOCAL_HEADER_SIZE);
  const localCompressedSize = header.readUInt32LE(18);
  const localSize = header.readUInt32LE(22);
  const nameSize = header.readUInt16LE(26);
  const metadataSize = nameSize + header.readUInt16LE(28);
  if (
    header.readUInt32LE(0) !== 0x04034b50 ||
    header.readUInt16LE(6) !== flags ||
    header.readUInt16LE(8) !== method ||
    // With a data descriptor, the sizes come from the central directory.
    (!(flags & 8) && (localCompressedSize !== compressedSize || localSize !== size)) ||
    nameSize !== name.length ||
    headerEnd + metadataSize + compressedSize > directoryOffset
  ) {
    throw new Error("invalid ZIP library header");
  }
  const bytes = await readRange(headerEnd, metadataSize + compressedSize);
  if (!bytes.subarray(0, nameSize).equals(name)) throw new Error("incorrect local ZIP entry name");
  checkEntryFields(bytes.subarray(nameSize, metadataSize), flags, localSize, localCompressedSize);
  const compressed = bytes.subarray(metadataSize);
  if (method === 0) return compressed;

  const output = Buffer.alloc(size);
  const inflate = createInflateRaw();
  inflate.end(compressed);
  let written = 0;
  for await (const chunk of inflate) {
    if (written + chunk.length > size)
      throw new Error("inflated ZIP library exceeds declared size");
    output.set(chunk, written);
    written += chunk.length;
  }
  if (written !== size) throw new Error("short inflated ZIP library");
  return output;
}
