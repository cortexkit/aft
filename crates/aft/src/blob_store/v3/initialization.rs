//! Atomic bootstrap of empty v3 SQLite files. Schema, application ID, seed rows
//! and the final format_meta singleton are published in one transaction.
//! SQLite's pointer-map mode is selected before that transaction begins.

use super::*;

fn has_marker(c: &Connection, path: &Path) -> Result<bool> {
    Ok(format_read(
        c.query_row("SELECT 1 FROM format_meta WHERE singleton=1", [], |r| {
            r.get::<_, i64>(0)
        }),
        path,
        "read initialization marker",
    )?
    .is_some())
}
fn has_any_seed(c: &Connection, path: &Path, kind: Kind) -> Result<bool> {
    let sql = match kind {
        Kind::Registry => "SELECT 1 FROM accounting WHERE singleton=1",
        Kind::Plane(_) => "SELECT 1 FROM store_meta WHERE singleton=1",
    };
    Ok(format_read(
        c.query_row(sql, [], |r| r.get::<_, i64>(0)),
        path,
        "read initialization seed",
    )?
    .is_some())
}
pub(super) fn has_seed(c: &Connection, kind: Kind) -> rusqlite::Result<bool> {
    match kind {
        Kind::Registry => c.query_row("SELECT 1 FROM accounting WHERE singleton=1", [], |_| {
            Ok(true)
        }),
        Kind::Plane(plane) => c
            .query_row("SELECT plane FROM store_meta WHERE singleton=1", [], |r| {
                r.get::<_, String>(0)
            })
            .map(|stored| stored == plane.as_str()),
    }
}

fn resumable(c: &Connection, path: &Path, kind: Kind) -> Result<bool> {
    let Some(rows) = format_read(schema_rows(c), path, "read bootstrap schema")? else {
        return Ok(false);
    };
    let app: i64 = c.pragma_query_value(None, "application_id", |r| r.get(0))?;
    if app != 0 && app != kind.app_id() {
        return Ok(false);
    }
    let vacuum: i64 = c.pragma_query_value(None, "auto_vacuum", |r| r.get(0))?;
    if vacuum != 2 && !(vacuum == 0 && rows.is_empty()) {
        return Ok(false);
    }
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(META)?;
    expected.execute_batch(kind.schema())?;
    let expected = schema_rows(&expected)?;
    if rows.iter().any(|row| !expected.contains(row)) {
        return Ok(false);
    }
    for (_, name, _, _) in rows.iter().filter(|row| row.0 == "table") {
        if matches!(name.as_str(), "format_meta" | "accounting" | "store_meta")
            && c.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM \"{name}\" WHERE singleton!=1)"),
                [],
                |row| row.get::<_, bool>(0),
            )?
        {
            return Ok(false);
        }
        match name.as_str() {
            "format_meta" => {
                let row:Option<(u32,u32,u32,String)>=c.query_row("SELECT format_version,schema_version,layout_tag,schema_text FROM format_meta WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
                if row.is_some_and(|(version, schema, layout, text)| {
                    version != FORMAT_VERSION
                        || schema != SCHEMA_VERSION
                        || layout != kind.layout()
                        || text != format!("{META}{}", kind.schema())
                }) {
                    return Ok(false);
                }
            }
            "accounting" => {
                let row: Option<(u64, u64)> = c
                    .query_row(
                        "SELECT measured_bytes,watermark FROM accounting WHERE singleton=1",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if row.is_some_and(|row| row != (0, 0)) {
                    return Ok(false);
                }
            }
            "store_meta" => {
                let row: Option<(String, i64)> = c
                    .query_row(
                        "SELECT plane,gc_epoch FROM store_meta WHERE singleton=1",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if row.is_some_and(|(stored,epoch)| !matches!(kind,Kind::Plane(plane) if stored==plane.as_str() && epoch==0)) {return Ok(false);}
            }
            _ => {
                let populated: bool = c.query_row(
                    &format!("SELECT EXISTS(SELECT 1 FROM \"{name}\")"),
                    [],
                    |r| r.get(0),
                )?;
                if populated {
                    return Ok(false);
                }
            }
        }
    }
    let sequence: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='sqlite_sequence')",
        [],
        |r| r.get(0),
    )?;
    if sequence
        && c.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_sequence WHERE seq>0)",
            [],
            |r| r.get::<_, bool>(0),
        )?
    {
        return Ok(false);
    }
    Ok(true)
}

pub(super) fn ensure(c: &mut IdentityConnection, path: &Path, kind: Kind) -> Result<()> {
    if has_marker(c, path)? && has_any_seed(c, path, kind)? {
        return Ok(());
    }
    if !resumable(c, path, kind)? {
        return Ok(());
    }
    // SQLite must select pointer-map mode before the first write transaction.
    // A kill here leaves only an empty, retryable bootstrap, not a format row.
    pragma(c, path, kind, "auto_vacuum", 2)?;
    #[cfg(test)]
    tests::initialization_hook(kind, "auto_vacuum");
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if has_marker(&tx, path)? && has_any_seed(&tx, path, kind)? {
        return Ok(());
    }
    if !resumable(&tx, path, kind)? {
        return Ok(());
    }
    // Old interrupted bootstraps can contain a prefix of the schema. Only
    // exact compiled objects with no application data are eligible for rebuild.
    for (_, name, _, _) in schema_rows(&tx)?.iter().filter(|row| row.0 == "table") {
        tx.execute_batch(&format!("DROP TABLE \"{name}\";"))?;
    }
    tx.execute_batch(META)?;
    #[cfg(test)]
    tests::initialization_hook(kind, "meta");
    tx.execute_batch(kind.schema())?;
    #[cfg(test)]
    tests::initialization_hook(kind, "schema");
    pragma(&tx, path, kind, "application_id", kind.app_id())?;
    #[cfg(test)]
    tests::initialization_hook(kind, "application_id");
    match kind {
        Kind::Registry => {
            tx.execute("INSERT INTO accounting VALUES(1,0,0)", [])?;
            #[cfg(test)]
            tests::initialization_hook(kind, "accounting");
        }
        Kind::Plane(plane) => {
            tx.execute("INSERT INTO store_meta VALUES(1,?1,0)", [plane.as_str()])?;
            #[cfg(test)]
            tests::initialization_hook(kind, "store_meta");
        }
    }
    tx.execute(
        "INSERT INTO format_meta VALUES(1,?1,?2,?3,?4)",
        params![
            FORMAT_VERSION,
            SCHEMA_VERSION,
            kind.layout(),
            format!("{META}{}", kind.schema())
        ],
    )?;
    #[cfg(test)]
    tests::initialization_hook(kind, "format");
    tx.commit()?;
    Ok(())
}
