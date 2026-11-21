use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub policies: usize,
    pub grants: usize,
    pub read_paths: usize,
}
type Shared = Arc<Mutex<Counts>>;
fn probes() -> &'static Mutex<HashMap<String, Shared>> {
    static PROBES: OnceLock<Mutex<HashMap<String, Shared>>> = OnceLock::new();
    PROBES.get_or_init(Mutex::default)
}
pub struct Probe {
    request: String,
    counts: Shared,
}
impl Probe {
    pub fn new(request: &str) -> Self {
        let counts = Arc::new(Mutex::new(Counts::default()));
        probes()
            .lock()
            .unwrap()
            .insert(request.to_owned(), counts.clone());
        Self {
            request: request.to_owned(),
            counts,
        }
    }
    pub fn counts(&self) -> Counts {
        *self.counts.lock().unwrap()
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        probes().lock().unwrap().remove(&self.request);
    }
}
pub(super) fn policy(request: &str) {
    if let Some(counts) = probes().lock().unwrap().get(request) {
        counts.lock().unwrap().policies += 1;
    }
}
pub(super) fn grants(request: &str) {
    if let Some(counts) = probes().lock().unwrap().get(request) {
        counts.lock().unwrap().grants += 1;
    }
}
pub(super) fn read_path(request: &str) {
    if let Some(counts) = probes().lock().unwrap().get(request) {
        counts.lock().unwrap().read_paths += 1;
    }
}
