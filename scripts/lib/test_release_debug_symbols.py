#!/usr/bin/env python3
"""Detached release symbols: identity checks and a real Linux split rehearsal."""

import importlib.util
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("release_debug_symbols", ROOT / "scripts/release-debug-symbols.py")
assert SPEC is not None and SPEC.loader is not None
symbols = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(symbols)


def pe_fixture(guid, age, name="aft-hash.pdb"):
    data = bytearray(1024)
    data[:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, 128)
    data[128:132] = b"PE\0\0"
    struct.pack_into("<H", data, 134, 1)
    struct.pack_into("<H", data, 148, 240)
    struct.pack_into("<H", data, 152, 0x20B)
    struct.pack_into("<II", data, 152 + 112 + 48, 0x1000, 28)
    struct.pack_into("<IIII", data, 152 + 240 + 8, 512, 0x1000, 512, 512)
    reference = b"RSDS" + guid + struct.pack("<I", age) + b"C:\\build\\" + name.encode() + b"\0"
    struct.pack_into("<IIII", data, 512 + 12, 2, len(reference), 0x1020, 544)
    data[544:544 + len(reference)] = reference
    return data


def pdb_fixture(guid, age):
    data = bytearray(512 * 5)
    data[:32] = b"Microsoft C/C++ MSF 7.00\r\n\x1aDS\0\0\0"
    struct.pack_into("<6I", data, 32, 512, 1, 5, 16, 0, 2)
    struct.pack_into("<I", data, 512 * 2, 3)
    struct.pack_into("<4I", data, 512 * 3, 2, 0xFFFFFFFF, 28, 4)
    struct.pack_into("<III", data, 512 * 4, 20000404, 0, age)
    data[512 * 4 + 12:512 * 4 + 28] = guid
    return data


class ReleaseDebugSymbolsTests(unittest.TestCase):
    def setUp(self):
        scratch = ROOT / "target/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)

    def test_windows_packages_pdb_under_executable_reference_name(self):
        guid = bytes(range(16))
        binary = self.directory / "aft.exe"
        pdb = self.directory / "aft.pdb"
        binary.write_bytes(pe_fixture(guid, 7))
        pdb.write_bytes(pdb_fixture(guid, 7))
        original = binary.read_bytes()
        archive = self.directory / "aft-windows-x64-pdb.zip"
        symbols.package_windows(binary, pdb, archive)
        with zipfile.ZipFile(archive) as output:
            self.assertEqual(output.namelist(), ["aft-hash.pdb"])
            self.assertEqual(output.read("aft-hash.pdb"), pdb.read_bytes())
        self.assertEqual(binary.read_bytes(), original)

    def test_windows_rejects_mismatched_pdb_guid_and_age(self):
        guid = bytes(range(16))
        binary = self.directory / "aft.exe"
        pdb = self.directory / "aft.pdb"
        binary.write_bytes(pe_fixture(guid, 7))
        for other_guid, other_age in ((bytes(16), 7), (guid, 8)):
            with self.subTest(guid=other_guid, age=other_age):
                pdb.write_bytes(pdb_fixture(other_guid, other_age))
                with self.assertRaisesRegex(ValueError, "GUID/age"):
                    symbols.package_windows(binary, pdb, self.directory / "bad.zip")
        self.assertFalse((self.directory / "bad.zip").exists())

    def test_windows_rejects_missing_executable_pdb_reference(self):
        data = pe_fixture(bytes(16), 1)
        data[544:548] = b"NONE"
        with self.assertRaisesRegex(ValueError, "no CodeView"):
            symbols.pe_pdb_reference(data)

    @unittest.skipUnless(sys.platform == "linux" and shutil.which("rustc"), "requires Linux rustc and binutils")
    def test_linux_split_preserves_code_and_symbolizes_without_runtime_debug_dependency(self):
        source = self.directory / "probe.rs"
        source.write_text(
            '#[no_mangle]\n#[inline(never)]\npub extern "C" fn release_symbol_probe(value: u64) -> u64 {\n'
            '    std::hint::black_box(value) * 3\n}\n'
            'fn main() { println!("{}", release_symbol_probe(7)); }\n'
        )
        binary = self.directory / "aft"
        subprocess.run([
            "rustc", "--edition=2021", str(source), "-o", str(binary), "-C", "opt-level=3",
            "-C", "lto=true", "-C", "codegen-units=1", "-C", "debuginfo=line-tables-only",
            "-C", "split-debuginfo=off", "-C", "strip=none",
        ], check=True)
        original_id = symbols.build_id(binary)
        dynamic = subprocess.check_output(["readelf", "-d", str(binary)])
        text = self.directory / "text.bin"
        subprocess.run(["objcopy", "--only-section=.text", "-O", "binary", str(binary), str(text)], check=True)
        original_code = text.read_bytes()
        baseline = self.directory / "baseline"
        subprocess.run(["objcopy", "--strip-all", str(binary), str(baseline)], check=True)
        archive = self.directory / "symbols/aft-linux-x64-debug.zip"
        symbols.package_linux(binary, archive, "objcopy")
        debug = archive.parent / "aft.debug"
        self.assertEqual(symbols.build_id(debug), original_id)
        with zipfile.ZipFile(archive) as output:
            self.assertEqual(output.namelist(), ["aft.debug"])
        names = subprocess.check_output(["nm", "--defined-only", str(debug)], text=True)
        address = next(line.split()[0] for line in names.splitlines() if line.endswith(" T release_symbol_probe"))
        location = subprocess.check_output(["addr2line", "-f", "-e", str(debug), address], text=True)
        print(f"addr2line: {location.strip()}")
        self.assertIn("release_symbol_probe", location)
        self.assertIn("probe.rs:4", location)
        subprocess.run(["objcopy", "--only-section=.text", "-O", "binary", str(binary), str(text)], check=True)
        self.assertEqual(text.read_bytes(), original_code)
        self.assertEqual(subprocess.check_output(["readelf", "-d", str(binary)]), dynamic)
        self.assertLessEqual(binary.stat().st_size, baseline.stat().st_size + 128)
        print(f"stripped sizes: baseline={baseline.stat().st_size}, debuglink={binary.stat().st_size}")
        shutil.rmtree(archive.parent)
        self.assertEqual(subprocess.check_output([str(binary)], text=True).strip(), "21")


if __name__ == "__main__":
    unittest.main()
