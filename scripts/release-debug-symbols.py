#!/usr/bin/env python3
"""Package detached release symbols and reject mismatched binary identities."""

import argparse
from pathlib import Path
import re
import struct
import subprocess
import uuid
import zipfile


def build_id(path):
    notes = subprocess.check_output(["readelf", "-n", str(path)], text=True, stderr=subprocess.PIPE)
    match = re.search(r"Build ID: ([0-9a-f]+)", notes)
    if not match:
        raise ValueError(f"{path}: missing ELF build ID")
    return match[1]


def package_linux(binary, archive, objcopy):
    archive.parent.mkdir(parents=True, exist_ok=True)
    debug = archive.parent / "aft.debug"
    original_id = build_id(binary)
    subprocess.run([objcopy, "--only-keep-debug", str(binary), str(debug)], check=True)
    # GNU debuglink is optional at runtime; the executable still runs without it.
    subprocess.run([objcopy, "--strip-all", str(binary)], check=True)
    subprocess.run([objcopy, f"--add-gnu-debuglink={debug}", str(binary)], check=True)
    if build_id(binary) != original_id or build_id(debug) != original_id:
        raise ValueError("ELF executable and detached debug file have different build IDs")
    sections = subprocess.check_output(["readelf", "-SW", str(binary)], text=True)
    debug_sections = subprocess.check_output(["readelf", "-SW", str(debug)], text=True, stderr=subprocess.PIPE)
    names = re.findall(r"\]\s+(\.\S+)", sections)
    # GNU strip retains Rust's tiny GDB auto-loader; it is not DWARF payload.
    embedded = [name for name in names if name == ".symtab" or (
        name.startswith((".debug_", ".zdebug_")) and name != ".debug_gdb_scripts"
    )]
    if embedded:
        raise ValueError(f"shipped ELF executable still contains debug sections: {embedded}")
    if ".debug_line " not in debug_sections or ".gnu_debuglink " not in sections:
        raise ValueError("missing detached line tables or executable debuglink")
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as output:
        output.write(debug, "aft.debug")
    print(f"Verified ELF build ID {original_id}: {binary} <-> {archive}")


def pe_pdb_reference(data):
    """Read the PE debug directory's PDB reference (GUID, age and filename)."""
    if data[:2] != b"MZ":
        raise ValueError("not a PE executable")
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe:pe + 4] != b"PE\0\0":
        raise ValueError("missing PE signature")
    count = struct.unpack_from("<H", data, pe + 6)[0]
    optional_size = struct.unpack_from("<H", data, pe + 20)[0]
    optional = pe + 24
    magic = struct.unpack_from("<H", data, optional)[0]
    if magic not in (0x10B, 0x20B):
        raise ValueError("unsupported PE optional header")
    directories = optional + (112 if magic == 0x20B else 96)
    debug_rva, debug_size = struct.unpack_from("<II", data, directories + 6 * 8)
    for index in range(count):
        section = optional + optional_size + index * 40
        virtual_size, rva, raw_size, raw_offset = struct.unpack_from("<IIII", data, section + 8)
        if rva <= debug_rva < rva + max(virtual_size, raw_size):
            offset = raw_offset + debug_rva - rva
            for entry in range(offset, offset + debug_size, 28):
                kind, size, _, pointer = struct.unpack_from("<IIII", data, entry + 12)
                if kind == 2 and data[pointer:pointer + 4] == b"RSDS":
                    guid = data[pointer + 4:pointer + 20]
                    age = struct.unpack_from("<I", data, pointer + 20)[0]
                    name = data[pointer + 24:pointer + size].split(b"\0", 1)[0].decode("utf-8")
                    return guid, age, name.replace("\\", "/").rsplit("/", 1)[-1]
    raise ValueError("executable has no CodeView RSDS PDB reference")


def pdb_identity(data):
    """Read GUID/age from stream 1 of an MSF 7.0 (modern MSVC) PDB."""
    if data[:32] != b"Microsoft C/C++ MSF 7.00\r\n\x1aDS\0\0\0":
        raise ValueError("not an MSF 7.0 PDB")
    block_size, _, _, directory_size, _, block_map = struct.unpack_from("<6I", data, 32)
    if block_size not in (512, 1024, 2048, 4096):
        raise ValueError("unsupported PDB block size")

    def read_blocks(blocks, size):
        return b"".join(data[block * block_size:(block + 1) * block_size] for block in blocks)[:size]

    def block_count(size):
        return (size + block_size - 1) // block_size

    directory_blocks = struct.unpack_from(f"<{block_count(directory_size)}I", data, block_map * block_size)
    directory = read_blocks(directory_blocks, directory_size)
    stream_count = struct.unpack_from("<I", directory)[0]
    if stream_count < 2:
        raise ValueError("PDB has no info stream")
    sizes = struct.unpack_from(f"<{stream_count}I", directory, 4)
    cursor = 4 + 4 * stream_count
    for index, size in enumerate(sizes[:2]):
        blocks = 0 if size == 0xFFFFFFFF else block_count(size)
        if index == 1:
            if size == 0xFFFFFFFF or size < 28:
                raise ValueError("PDB info stream is missing or truncated")
            info = read_blocks(struct.unpack_from(f"<{blocks}I", directory, cursor), size)
            age = struct.unpack_from("<I", info, 8)[0]
            return info[12:28], age
        cursor += blocks * 4
    raise ValueError("PDB info stream is missing")


def package_windows(binary, pdb, archive):
    guid, age, name = pe_pdb_reference(binary.read_bytes())
    if not name.lower().endswith(".pdb") or name in (".pdb", "..pdb"):
        raise ValueError(f"invalid CodeView PDB filename: {name}")
    if pdb_identity(pdb.read_bytes()) != (guid, age):
        raise ValueError("executable CodeView GUID/age does not match PDB info stream")
    archive.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as output:
        # Cargo copies a hash-named deps PDB to aft.pdb; debuggers need the name
        # recorded by the linker, not the copy's filename.
        output.write(pdb, name)
    print(f"Verified PDB {uuid.UUID(bytes_le=guid)} age {age} ({name}): {binary} <-> {archive}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="platform", required=True)
    linux = sub.add_parser("linux")
    linux.add_argument("--objcopy", default="objcopy")
    windows = sub.add_parser("windows")
    windows.add_argument("--pdb", type=Path, required=True)
    for command in (linux, windows):
        command.add_argument("--binary", type=Path, required=True)
        command.add_argument("--archive", type=Path, required=True)
    args = parser.parse_args()
    if args.platform == "linux":
        package_linux(args.binary, args.archive, args.objcopy)
    else:
        package_windows(args.binary, args.pdb, args.archive)


if __name__ == "__main__":
    main()
