#!/usr/bin/env python3
"""Attribute live heap in a non-inverted `malloc_history -callTree` report.

Finds every frame whose symbol contains ANCHOR (default: the incremental
refresh function) and prints, per anchor call site (source line), its live
bytes plus the children one and two levels below it. Anchor frames with
different return addresses are listed separately, which is what separates
"extract changed files" from "clone extracts" from "parse dependents" inside
one function.

usage: callgraph-large-refresh-malloc-tree.py REPORT [ANCHOR] [MIN_MIB]
"""
import re
import sys

LINE = re.compile(r"^(\s*[ +!:|]*?)(\d+) \(([\d.]+)([KMG]?)\) (.*)$")
MIB = {"": 1 / 2**20, "K": 1 / 1024, "M": 1, "G": 1024}


def parse(line):
    match = LINE.match(line)
    if not match:
        return None
    return (
        len(match.group(1)),
        int(match.group(2)),
        float(match.group(3)) * MIB[match.group(4)],
        match.group(5),
    )


def location(symbol):
    match = re.search(r"(\S+\.rs:\d+)\s*$", symbol)
    return match.group(1) if match else ""


def name(symbol):
    symbol = symbol.split("  (in ")[0]
    idents = re.findall(r"\d+([A-Za-z_][A-Za-z0-9_]{2,})", symbol)
    return "::".join(idents[-2:])


def main():
    report = sys.argv[1]
    anchor = sys.argv[2] if len(sys.argv) > 2 else "refresh_files_profiled_with"
    min_mib = float(sys.argv[3]) if len(sys.argv) > 3 else 20
    entries = [parse(line) for line in open(report, errors="replace").read().splitlines()]
    for index, entry in enumerate(entries):
        if not entry or anchor not in entry[3] or entry[2] < min_mib:
            continue
        indent = entry[0]
        print(f"{entry[2]:9.1f} MiB {entry[1]:>10} allocs  anchor @{location(entry[3])}")
        for child in entries[index + 1:]:
            if not child:
                continue
            if child[0] <= indent:
                break
            depth = (child[0] - indent) // 2
            if depth in (1, 2) and child[2] >= min_mib:
                pad = "  " * depth
                print(f"{child[2]:9.1f} MiB {child[1]:>10} allocs  {pad}{name(child[3])} @{location(child[3])}")


if __name__ == "__main__":
    main()
