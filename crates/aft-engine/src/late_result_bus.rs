use std::{path::PathBuf, sync::Arc};

use crate::{context::App, db::TrackedConnection};

pub use aft_shell::late_result_bus::*;

struct SharedDatabase {
    app: Arc<App>,
    path: PathBuf,
}

impl LedgerDatabase for SharedDatabase {
    fn with_aft_db(
        &self,
        read: &mut dyn FnMut(&TrackedConnection) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        let db = self
            .app
            .open_db(&self.path)
            .map_err(|_| "bus_ledger_open")?;
        let conn = db.try_lock().map_err(|_| "bus_ledger_busy")?;
        read(&conn)
    }
}

#[doc(hidden)]
pub fn start(app: Arc<App>, connection: PathBuf, database: PathBuf) -> Option<Worker> {
    let database = Arc::new(SharedDatabase {
        app,
        path: database,
    });
    aft_shell::late_result_bus::start(database, connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_result_bus_first_open_refreshes_engram_catalog_once() {
        let (_fixture, storage) = aft_base::engram_catalog::tests::storage_root();
        let database = SharedDatabase {
            app: App::default_shared(),
            path: storage.join("aft.db"),
        };
        let catalog = storage.join(crate::engram_catalog::CATALOG_FILENAME);
        assert!(!database.path.exists());
        assert!(!catalog.exists());

        assert_eq!(scan_from_start_for_test(&database), Ok(0));
        let contents: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&catalog).unwrap()).unwrap();
        assert!(contents["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["path"] == "aft.db"));

        // A second scan should reuse the resident connection without refreshing
        // an unchanged storage root. Removing the catalog makes a refresh visible.
        std::fs::remove_file(&catalog).unwrap();
        assert_eq!(scan_from_start_for_test(&database), Ok(0));
        assert!(!catalog.exists());
    }
}
