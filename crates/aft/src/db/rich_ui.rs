pub use aft_db::db::rich_ui::*;

#[cfg(test)]
#[allow(unused_imports)]
pub use super::remote_exec::PolicyKey as PresetKey;
#[cfg(test)]
#[allow(unused_imports)]
use rusqlite::{params, Connection};

#[cfg(test)]
mod tests {
    use aft_db::db::rich_ui::tests::*;

    use super::*;
    use rusqlite::Connection;

    #[test]
    fn rich_ui_presets_retention_tick_removes_only_older_than_thirty_days() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::super::run_migrations(&mut conn).unwrap();
        let now = 40 * 24 * 60 * 60 * 1000;
        let cutoff = now - 30 * 24 * 60 * 60 * 1000;
        record_preset(&conn, &key(1), cutoff - 1).unwrap();
        record_preset(&conn, &key(2), cutoff).unwrap();
        record_preset(&conn, &key(3), now).unwrap();
        crate::bash_background::persistence::retention::prune_retention_tick(&mut conn, now)
            .unwrap();
        assert!(session_presets(&conn, &key(1)).unwrap().is_empty());
        assert_eq!(session_presets(&conn, &key(2)).unwrap(), ["worker"]);
        assert_eq!(session_presets(&conn, &key(3)).unwrap(), ["worker"]);
    }
}
