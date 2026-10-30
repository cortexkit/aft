//! Preserve rowids and untouched index entries when replacing a changed file.

use rusqlite::types::{ToSqlOutput, Value};
use rusqlite::{params_from_iter, Connection, OptionalExtension, ToSql};

pub(super) fn write(
    connection: &Connection,
    table: &str,
    columns: &[&str],
    parameters: &[&dyn ToSql],
    incremental: bool,
) -> rusqlite::Result<usize> {
    let values = parameters
        .iter()
        .map(|parameter| match parameter.to_sql()? {
            ToSqlOutput::Borrowed(value) => Ok(Value::from(value)),
            ToSqlOutput::Owned(value) => Ok(value),
            _ => Err(rusqlite::Error::InvalidQuery),
        })
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let key = columns[0];
    if incremental {
        connection
            .prepare_cached(
                "INSERT OR IGNORE INTO emitted_view_rows(table_name, row_key) VALUES (?1, ?2)",
            )?
            .execute(rusqlite::params![table, values[0]])?;
        let old = connection
            .prepare_cached(&format!(
                "SELECT {} FROM {table} WHERE {key} = ?1",
                columns.join(",")
            ))?
            .query_row([&values[0]], |row| {
                (0..columns.len())
                    .map(|index| row.get::<_, Value>(index))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .optional()?;
        if let Some(old) = old {
            let changed = (1..columns.len())
                .filter(|&index| old[index] != values[index])
                .collect::<Vec<_>>();
            if !changed.is_empty() {
                // Listing an unchanged indexed column in UPDATE still asks SQLite
                // to maintain its index. Update only the fields that really differ.
                let assignments = changed
                    .iter()
                    .enumerate()
                    .map(|(slot, &index)| format!("{}=?{}", columns[index], slot + 2))
                    .collect::<Vec<_>>()
                    .join(",");
                connection
                    .prepare_cached(&format!(
                        "UPDATE OR REPLACE {table} SET {assignments} WHERE {key}=?1"
                    ))?
                    .execute(params_from_iter(
                        std::iter::once(&values[0])
                            .chain(changed.iter().map(|&index| &values[index])),
                    ))?;
            }
            // MaterializeStats counts logical replacement rows, not SQL writes.
            return Ok(1);
        }
    }
    let placeholders = (1..=columns.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(",");
    connection
        .prepare_cached(&format!(
            "INSERT OR REPLACE INTO {table} ({}) VALUES ({placeholders})",
            columns.join(",")
        ))?
        .execute(params_from_iter(&values))
}

pub(super) fn prune(connection: &Connection) -> rusqlite::Result<()> {
    // Keep old refs until edge ownership has been checked. Dispatch reconciles
    // its own edges after the ordinary graph has reached its new state.
    connection.execute_batch(
        "DELETE FROM edges WHERE ref_id IN (
             SELECT refs.ref_id FROM changed_view_paths
             CROSS JOIN refs INDEXED BY idx_refs_caller_file
                 ON refs.caller_file = changed_view_paths.path
         ) AND edge_id NOT IN (
             SELECT row_key FROM emitted_view_rows WHERE table_name='edges'
         );
         DELETE FROM refs WHERE caller_file IN (SELECT path FROM changed_view_paths)
             AND ref_id NOT IN (SELECT row_key FROM emitted_view_rows WHERE table_name='refs');
         DELETE FROM nodes WHERE file_path IN (SELECT path FROM changed_view_paths)
             AND id NOT IN (SELECT row_key FROM emitted_view_rows WHERE table_name='nodes');
         DELETE FROM files WHERE path IN (SELECT path FROM changed_view_paths)
             AND path NOT IN (SELECT row_key FROM emitted_view_rows WHERE table_name='files');",
    )
}
