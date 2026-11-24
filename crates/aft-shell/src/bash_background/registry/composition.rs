use super::*;

impl BgTaskRegistry {
    #[doc(hidden)]
    pub fn shares_db_pool(&self, conn: &Arc<Mutex<TrackedConnection>>) -> bool {
        self.uses_db_pool(conn)
    }

    #[doc(hidden)]
    pub fn has_running_tasks(&self) -> bool {
        !self.running_tasks().is_empty()
    }

    #[doc(hidden)]
    pub fn try_any_running_tasks(&self) -> Option<bool> {
        self.try_has_running_tasks()
    }
}
