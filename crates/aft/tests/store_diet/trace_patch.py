#!/usr/bin/env python3
"""Write (do not apply) the trace-only patch against the recorded baseline.

The wrapper preserves rusqlite's result and open flags. The environment switch
is read only by this instrumentation; no production SQL or reply is changed.
"""
import difflib
import pathlib
import re

from inventory import HERE, ROOT

HELPER = '''//! SQL capture for offline baseline replay, never part of a release build.
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

fn install(result: rusqlite::Result<Connection>) -> rusqlite::Result<Connection> {
    let mut connection = result?;
    if std::env::var_os("AFT_STORE_DIET_SQL_TRACE").is_some() {
        connection.trace(Some(|sql| {
            use std::io::Write;
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let _guard = LOCK.lock().unwrap();
            let path = std::env::var_os("AFT_STORE_DIET_SQL_TRACE").unwrap();
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
            writeln!(file, "{}", serde_json::to_string(sql).unwrap()).unwrap();
        }));
    }
    Ok(connection)
}

pub(crate) fn open(path: impl AsRef<Path>) -> rusqlite::Result<Connection> {
    install(Connection::open(path))
}

pub(crate) fn open_with_flags(path: impl AsRef<Path>, flags: OpenFlags) -> rusqlite::Result<Connection> {
    install(Connection::open_with_flags(path, flags))
}

pub(crate) fn open_with_flags_and_vfs(path: impl AsRef<Path>, flags: OpenFlags, vfs: &str) -> rusqlite::Result<Connection> {
    install(Connection::open_with_flags_and_vfs(path, flags, vfs))
}

pub(crate) fn open_in_memory() -> rusqlite::Result<Connection> {
    install(Connection::open_in_memory())
}
'''


def main():
    chunks = []
    sites = []
    for path in sorted((ROOT / 'crates/aft/src').rglob('*.rs')):
        before = path.read_text()
        # Both tracked connection types ultimately call these raw open seams.
        # Match the complete type name, not IdentityConnection/TrackedConnection.
        after, count = re.subn(r'\b(?:rusqlite::)?Connection::(open(?:_with_flags_and_vfs|_with_flags|_in_memory)?)\(', r'crate::db::store_diet_trace::\1(', before)
        if path == ROOT / 'crates/aft/src/db/mod.rs':
            after += '\npub(crate) mod store_diet_trace;\n'
        if before != after:
            relative = str(path.relative_to(ROOT))
            chunks.extend(difflib.unified_diff(before.splitlines(True), after.splitlines(True), 'a/' + relative, 'b/' + relative))
            if count:
                sites.append({'path': relative, 'raw_open_calls': count})
    relative = 'crates/aft/src/db/store_diet_trace.rs'
    chunks.extend(difflib.unified_diff([], HELPER.splitlines(True), '/dev/null', 'b/' + relative))
    (HERE / 'baseline-trace.patch').write_text(''.join(chunks))
    from inventory import dump
    dump(HERE / 'trace_helpers.json', sites)
    print(f'captured {sum(s["raw_open_calls"] for s in sites)} raw opens in {len(sites)} files')


if __name__ == '__main__':
    main()
