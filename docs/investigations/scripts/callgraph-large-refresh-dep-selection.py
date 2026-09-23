#!/usr/bin/env python3
"""Replay the incremental refresh's dependent-ref selection SQL on a store.

For every path in the list that the store indexes, run the same statement as
`ref_ids_depending_on` and report rows returned, SQL wall time, and this
process's logical/physical write deltas (Darwin rusage_info_v4), which isolate
SQLite temporary-sorter traffic from the Rust-side filtering.

usage: dep-selection-replay.py DB PATHS_FILE
"""
import ctypes
import sqlite3
import sys
import time

QUERY = """
SELECT DISTINCT r.ref_id, r.kind, r.caller_file, r.module_path, r.target_file
FROM refs r
WHERE r.caller_file IN (SELECT file_path FROM file_dependencies WHERE dep_file = ?1)
   OR r.target_file = ?1
ORDER BY r.ref_id
"""

libc = ctypes.CDLL(None)


def writes():
    buf = (ctypes.c_uint64 * 64)()
    import os
    libc.proc_pid_rusage(os.getpid(), 4, ctypes.byref(buf))
    # 16-byte uuid = 2 u64 slots; field 17 = diskio_byteswritten, 27 = logical_writes
    return buf[2 + 17], buf[2 + 27]


def main():
    db, paths_file = sys.argv[1], sys.argv[2]
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    indexed = {row[0] for row in conn.execute("SELECT path FROM files")}
    paths = [p for p in open(paths_file).read().splitlines() if p in indexed]
    plan = conn.execute("EXPLAIN QUERY PLAN " + QUERY, (paths[0],)).fetchall()
    print("plan:", [row[-1] for row in plan])
    phys0, log0 = writes()
    started = time.time()
    rows = 0
    kinds = {}
    for path in paths:
        for ref_id, kind, caller, module, target in conn.execute(QUERY, (path,)):
            rows += 1
            kinds[kind] = kinds.get(kind, 0) + 1
    elapsed = time.time() - started
    phys1, log1 = writes()
    print(f"queried_paths={len(paths)} rows_returned={rows} sql_seconds={elapsed:.1f} "
          f"physical_written_mib={(phys1 - phys0) / 2**20:.1f} logical_written_mib={(log1 - log0) / 2**20:.1f}")
    print("rows_by_kind:", dict(sorted(kinds.items(), key=lambda kv: -kv[1])))


if __name__ == "__main__":
    main()
