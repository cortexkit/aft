//! Process-wide deadlines. Callbacks only check state or enqueue work: blocking
//! I/O and cache destruction belong on the caller's existing work pool.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

type Callback = Box<dyn FnMut() -> Option<Instant> + Send>;
type Key = (Instant, u64);

struct Entry {
    active: Arc<AtomicBool>,
    fire: Callback,
}

#[derive(Default)]
struct Queue {
    next_id: u64,
    deadlines: BTreeMap<Key, Entry>,
    keys: HashMap<u64, Key>,
}

#[derive(Default)]
struct Service {
    queue: Mutex<Queue>,
    wake: Condvar,
}

fn service() -> io::Result<&'static Arc<Service>> {
    static SERVICE: OnceLock<Result<Arc<Service>, String>> = OnceLock::new();
    SERVICE
        .get_or_init(|| {
            let service = Arc::new(Service::default());
            let worker = Arc::clone(&service);
            std::thread::Builder::new()
                .name("aft-shared-timer".into())
                .spawn(move || worker.run())
                .map_err(|error| error.to_string())?;
            Ok(service)
        })
        .as_ref()
        .map_err(|error| io::Error::other(error.clone()))
}

impl Service {
    fn run(&self) {
        loop {
            let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            let Some((&(at, id), _)) = queue.deadlines.first_key_value() else {
                drop(self.wake.wait(queue).unwrap_or_else(|e| e.into_inner()));
                continue;
            };
            let now = Instant::now();
            if now < at {
                drop(
                    self.wake
                        .wait_timeout(queue, at - now)
                        .unwrap_or_else(|e| e.into_inner()),
                );
                continue;
            }
            let mut entry = queue.deadlines.remove(&(at, id)).unwrap();
            queue.keys.remove(&id);
            drop(queue);
            if !entry.active.load(Ordering::Acquire) {
                continue;
            }
            // A bad callback must not disable every root's deadlines.
            let next = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut entry.fire));
            if let Ok(Some(at)) = next {
                let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
                if entry.active.load(Ordering::Acquire) {
                    queue.keys.insert(id, (at, id));
                    queue.deadlines.insert((at, id), entry);
                }
            }
        }
    }
}

/// Cancels a pending deadline on drop, removing its callback immediately. A
/// callback already being dispatched must also fence its work with the root's
/// lifecycle (usually a weak owner and the executor's cancellation token).
pub(crate) struct Timer {
    service: &'static Arc<Service>,
    id: u64,
    active: Arc<AtomicBool>,
}

impl Timer {
    #[cfg(test)]
    pub(crate) fn registration_for_test(&self) -> u64 {
        self.id
    }

    #[cfg(test)]
    pub(crate) fn registered_for_test(id: u64) -> bool {
        service()
            .unwrap()
            .queue
            .lock()
            .unwrap()
            .keys
            .contains_key(&id)
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn deadline_for_test(id: u64) -> Option<Instant> {
        service()
            .unwrap()
            .queue
            .lock()
            .unwrap()
            .keys
            .get(&id)
            .map(|key| key.0)
    }

    pub(crate) fn at(
        at: Instant,
        fire: impl FnMut() -> Option<Instant> + Send + 'static,
    ) -> io::Result<Self> {
        let service = service()?;
        let active = Arc::new(AtomicBool::new(true));
        let mut queue = service.queue.lock().unwrap_or_else(|e| e.into_inner());
        let id = queue.next_id;
        queue.next_id = queue.next_id.checked_add(1).expect("timer id exhausted");
        queue.keys.insert(id, (at, id));
        queue.deadlines.insert(
            (at, id),
            Entry {
                active: Arc::clone(&active),
                fire: Box::new(fire),
            },
        );
        service.wake.notify_one();
        Ok(Self {
            service,
            id,
            active,
        })
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        let mut queue = self.service.queue.lock().unwrap_or_else(|e| e.into_inner());
        let removed = queue
            .keys
            .remove(&self.id)
            .and_then(|key| queue.deadlines.remove(&key));
        self.service.wake.notify_one();
        drop(queue);
        drop(removed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn deadlines_rearm_cancel_and_survive_panics() {
        let (tx, rx) = mpsc::channel();
        let start = Instant::now();
        let mut count = 0;
        let _timer = Timer::at(start, move || {
            count += 1;
            tx.send((count, Instant::now())).unwrap();
            (count == 1).then_some(start + Duration::from_millis(30))
        })
        .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().0, 1);
        let (count, at) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(count, 2);
        assert!(at >= start + Duration::from_millis(30));

        let (tx, rx) = mpsc::channel();
        let timer = Timer::at(Instant::now() + Duration::from_millis(30), move || {
            tx.send(()).unwrap();
            None
        })
        .unwrap();
        drop(timer);
        assert!(rx.recv_timeout(Duration::from_millis(60)).is_err());

        let _panic = Timer::at(Instant::now(), || panic!("isolated callback panic")).unwrap();
        let (tx, rx) = mpsc::channel();
        let _later = Timer::at(Instant::now() + Duration::from_millis(10), move || {
            tx.send(()).unwrap();
            None
        })
        .unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }
}
