//! Database work for display delivery runs off the async executor and reply path.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

use crate::context::App;
use crate::db::display_records;

pub(super) struct Store {
    app: Arc<App>,
    database: PathBuf,
    pending_awaited: Arc<AtomicUsize>,
    deferred_awaited: parking_lot::Mutex<HashMap<String, (Instant, Duration)>>,
}

impl Store {
    pub(super) fn new(app: Arc<App>, database: PathBuf) -> Self {
        Self {
            app,
            database,
            pending_awaited: Default::default(),
            deferred_awaited: Default::default(),
        }
    }

    async fn run<T: Send + 'static>(
        &self,
        action: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, &'static str> {
        let app = self.app.clone();
        let database = self.database.clone();
        tokio::task::spawn_blocking(move || {
            let db = app.open_db(&database).map_err(|_| "display_store_open")?;
            let conn = db.lock().map_err(|_| "display_store_poisoned")?;
            action(&conn).map_err(|_| "display_store_query")
        })
        .await
        .map_err(|_| "display_store_worker")?
    }

    pub(super) async fn persist(&self, id: String, bytes: Vec<u8>) -> Result<bool, &'static str> {
        self.run(move |conn| display_records::persist(conn, &id, &bytes, display_records::now_ms()))
            .await
    }

    pub(super) async fn acknowledged(&self, id: String) -> Result<(), &'static str> {
        self.run(move |conn| display_records::acknowledged(conn, &id, display_records::now_ms()))
            .await
    }

    pub(super) async fn prepare_awaited_notice(
        &self,
        id: String,
        bytes: Vec<u8>,
    ) -> Result<bool, &'static str> {
        self.run(move |conn| {
            display_records::prepare_awaited_notice(conn, &id, &bytes, display_records::now_ms())
        })
        .await
    }

    pub(super) async fn activate_awaited_notice(&self, id: String) -> Result<(), &'static str> {
        let pending = self.pending_awaited.clone();
        self.run(move |conn| {
            let now = display_records::now_ms();
            display_records::activate_awaited_notice(conn, &id, now)?;
            pending.store(
                display_records::count_pending_awaited_notices(conn, now)?,
                Ordering::Release,
            );
            Ok(())
        })
        .await
    }

    pub(super) async fn commit_awaited_notice(&self, id: String) -> Result<bool, &'static str> {
        self.run(move |conn| {
            display_records::commit_awaited_notice(conn, &id, display_records::now_ms())
        })
        .await
    }

    pub(super) async fn discard_unlinked_notice(&self, id: String) -> Result<(), &'static str> {
        self.run(move |conn| display_records::discard_unlinked_notice(conn, &id))
            .await
    }

    pub(super) async fn restore_awaited_count(&self) -> Result<usize, &'static str> {
        let pending = self.pending_awaited.clone();
        self.run(move |conn| {
            let count =
                display_records::count_pending_awaited_notices(conn, display_records::now_ms())?;
            pending.store(count, Ordering::Release);
            Ok(count)
        })
        .await
    }

    pub(super) async fn pending_awaited_notice(&self) -> Result<Option<String>, &'static str> {
        if self.pending_awaited.load(Ordering::Acquire) == 0 {
            return Ok(None);
        }
        let now = Instant::now();
        let deferred = self
            .deferred_awaited
            .lock()
            .iter()
            .filter_map(|(id, (next, _))| (*next > now).then(|| id.clone()))
            .collect::<Vec<_>>();
        let pending = self.pending_awaited.clone();
        self.run(move |conn| {
            let now = display_records::now_ms();
            let selected = display_records::pending_awaited_notice(conn, now, &deferred)?;
            if selected.is_none() {
                pending.store(
                    display_records::count_pending_awaited_notices(conn, now)?,
                    Ordering::Release,
                );
            }
            Ok(selected)
        })
        .await
    }

    pub(super) async fn acknowledged_awaited_notice(&self, id: String) -> Result<(), &'static str> {
        let pending = self.pending_awaited.clone();
        let stored_id = id.clone();
        self.run(move |conn| {
            let now = display_records::now_ms();
            display_records::acknowledged(conn, &stored_id, now)?;
            pending.store(
                display_records::count_pending_awaited_notices(conn, now)?,
                Ordering::Release,
            );
            Ok(())
        })
        .await?;
        self.deferred_awaited.lock().remove(&id);
        Ok(())
    }

    pub(super) fn defer_awaited_notice(&self, id: String) {
        let now = Instant::now();
        let mut deferred = self.deferred_awaited.lock();
        deferred
            .retain(|_, (next, _)| now.saturating_duration_since(*next) < Duration::from_secs(60));
        let entry = deferred
            .entry(id)
            .or_insert((now, Duration::from_millis(100)));
        entry.0 = now + entry.1;
        entry.1 = (entry.1 * 2).min(Duration::from_secs(5));
    }

    pub(super) async fn read(
        &self,
        id: String,
        at_ms: i64,
    ) -> Result<Vec<u8>, subc_protocol::ErrorBody> {
        match self
            .run(move |conn| display_records::read(conn, &id, at_ms))
            .await
        {
            Ok(Some(bytes)) => Ok(bytes),
            Ok(None) => Err(subc_protocol::ErrorBody::new(
                cortexkit_role_tool_display::DISPLAY_RECORD_UNKNOWN,
                "display record is unknown",
            )),
            Err(code) => {
                log::warn!("display pull store unavailable: {code}");
                Err(cortexkit_role_tool_display::not_ready())
            }
        }
    }
}
