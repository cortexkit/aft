use aft_db::db::TrackedConnection;

/// Runs ledger reads on the runtime's owning AFT database connection.
#[doc(hidden)]
pub trait LedgerDatabase {
    /// Calls `read` once, synchronously, only if the connection is available.
    fn with_aft_db(
        &self,
        read: &mut dyn FnMut(&TrackedConnection) -> Result<(), &'static str>,
    ) -> Result<(), &'static str>;
}

pub(super) fn read<T>(
    database: &dyn LedgerDatabase,
    read: impl FnOnce(&TrackedConnection) -> Result<T, &'static str>,
) -> Result<T, &'static str> {
    let mut read = Some(read);
    let mut result = None;
    database.with_aft_db(&mut |conn| {
        result = Some(read.take().expect("ledger read invoked twice")(conn)?);
        Ok(())
    })?;
    Ok(result.expect("ledger read not invoked"))
}
