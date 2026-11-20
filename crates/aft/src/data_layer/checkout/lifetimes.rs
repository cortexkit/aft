use super::*;

impl Drop for BuildReservation {
    fn drop(&mut self) {
        self.live
            .write()
            .expect("no panics while holding it")
            .building = false;
    }
}

#[cfg(test)]
impl Drop for CallGraphHold {
    fn drop(&mut self) {
        call_graph_holds_for_test()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

#[cfg(test)]
impl Drop for PublicationHold {
    fn drop(&mut self) {
        publication_holds_for_test()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}
