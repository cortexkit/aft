//! One SQLite-owned connection per live v3 file identity. A read capability
//! shares its owner's connection instead of opening or closing another file.

use super::*;
use crate::db::file_identity::{identity_of, FileIdentity, IdentityConnection};
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, MutexGuard, OnceLock, Weak};

enum Slot {
    Open {
        connection: IdentityConnection,
        writable: bool,
    },
    Closed,
}

pub(super) struct Database {
    identity: Option<FileIdentity>,
    kind: Kind,
    slot: Mutex<Slot>,
}

pub(super) struct Guard<'a>(MutexGuard<'a, Slot>);
impl Deref for Guard<'_> {
    type Target = IdentityConnection;
    fn deref(&self) -> &Self::Target {
        match &*self.0 {
            Slot::Open { connection, .. } => connection,
            Slot::Closed => unreachable!("guard is created only for an open slot"),
        }
    }
}
impl DerefMut for Guard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match &mut *self.0 {
            Slot::Open { connection, .. } => connection,
            Slot::Closed => unreachable!("guard is created only for an open slot"),
        }
    }
}
impl Database {
    pub(super) fn connection(&self) -> Result<Guard<'_>> {
        let slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*slot, Slot::Closed) {
            return Err(Error::Refused("reservation_ledger_unreadable"));
        }
        Ok(Guard(slot))
    }
    fn prepare(&self, route: &Path, create: bool, readonly: bool) -> Result<()> {
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let reopen = matches!(*slot, Slot::Closed)
            || (!readonly
                && matches!(
                    *slot,
                    Slot::Open {
                        writable: false,
                        ..
                    }
                ));
        if reopen {
            // A read-only SQLite handle cannot become writable. Close it while
            // the family is exclusively held, before opening its replacement.
            // Existing read capabilities still refer to this Database object,
            // so later operations use its replacement connection.
            *slot = Slot::Closed;
            let connection = open_db(route, self.kind, create, readonly)?;
            *slot = Slot::Open {
                connection,
                writable: !readonly,
            };
        } else if let Slot::Open { connection, .. } = &*slot {
            validate(connection, route, self.kind)?;
        }
        Ok(())
    }
}

fn databases() -> &'static Mutex<HashMap<PathBuf, Weak<Database>>> {
    static DATABASES: OnceLock<Mutex<HashMap<PathBuf, Weak<Database>>>> = OnceLock::new();
    DATABASES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn shared(
    path: &Path,
    kind: Kind,
    create: bool,
    readonly: bool,
) -> Result<Arc<Database>> {
    let canonical = if path
        .try_exists()
        .map_err(|e| crate::views::io::at("checking v3 database", path, e))?
    {
        fs::canonicalize(path)
            .map_err(|e| crate::views::io::at("resolving v3 database", path, e))?
    } else {
        let parent = path.parent().ok_or(Error::InvalidIdentity)?;
        fs::canonicalize(parent)
            .map_err(|e| crate::views::io::at("resolving v3 database directory", parent, e))?
            .join(path.file_name().ok_or(Error::InvalidIdentity)?)
    };
    let identity = identity_of(&canonical);
    let mut cache = databases()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.retain(|_, database| database.strong_count() > 0);
    let existing = cache.get(&canonical).and_then(Weak::upgrade).or_else(|| {
        identity.and_then(|identity| {
            cache
                .values()
                .filter_map(Weak::upgrade)
                .find(|database| database.identity == Some(identity))
        })
    });
    if let Some(database) = existing {
        if database.kind != kind
            || (identity.is_some() && database.identity.is_some() && identity != database.identity)
        {
            return Err(Error::Refused("live_sqlite_file_replaced"));
        }
        // Never wait for a database connection mutex while holding the global
        // cache: admission can hold one connection while looking up another ledger.
        drop(cache);
        database.prepare(path, create, readonly)?;
        return Ok(database);
    }
    // The cache lock spans creation so two first opens cannot acquire separate
    // handles. Only SQLite opens the database and its companion files.
    let connection = open_db(path, kind, create, readonly)?;
    let database = Arc::new(Database {
        identity: identity_of(&canonical),
        kind,
        slot: Mutex::new(Slot::Open {
            connection,
            writable: !readonly,
        }),
    });
    cache.insert(canonical, Arc::downgrade(&database));
    Ok(database)
}
