//! Database work for display delivery runs off the async executor and reply path.
use std::path::PathBuf;
use std::sync::Arc;

use crate::context::App;
use crate::db::display_records;

pub(super) struct Store {
    app: Arc<App>,
    database: PathBuf,
}

impl Store {
    pub(super) fn new(app: Arc<App>, database: PathBuf) -> Self {
        Self { app, database }
    }

    async fn run<T: Send + 'static>(
        &self,
        action: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, &'static str> {
        let app = self.app.clone();
        let database = self.database.clone();
        tokio::task::spawn_blocking(move || {
            let db = app.open_db(&database).map_err(|_| "display_store_open")?;
            let conn = db.try_lock().map_err(|_| "display_store_busy")?;
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
