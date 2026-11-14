pub use aft_base::secret::consent::*;

#[cfg(test)]
#[allow(unused_imports)]
use crate::secret::SecretScope;

#[cfg(test)]
mod waiter_tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn a_last_waiter_cannot_decrement_or_cancel_while_a_join_holds_the_cards_lock() {
        let card = Arc::new(SharedCard {
            created: Instant::now(),
            statement: "statement".into(),
            expires: u64::MAX,
            outcome: tokio::sync::watch::channel(None).0,
            outcome_code: Default::default(),
            cancel: Default::default(),
            waiters: AtomicUsize::new(1),
        });
        let cards = CARDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let leaving = card.clone();
        let (started, start) = std::sync::mpsc::channel();
        let (finished, finish) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            drop(Waiter(leaving));
            finished.send(()).unwrap();
        });
        start.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            matches!(
                finish.recv_timeout(Duration::from_millis(100)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "the decrement/cancel decision must wait for the joining critical section"
        );
        assert_eq!(card.waiters.load(Ordering::Acquire), 1);
        assert!(!card.cancel.is_cancelled());
        card.waiters.fetch_add(1, Ordering::AcqRel);
        drop(cards);
        finish.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert_eq!(card.waiters.load(Ordering::Acquire), 1);
        assert!(
            !card.cancel.is_cancelled(),
            "the already-counted join still owns the card"
        );
        drop(Waiter(card.clone()));
        assert!(card.cancel.is_cancelled());
        assert_eq!(
            card.cancel.reason(),
            Some(CancellationReason::ClientDropped)
        );
    }
}

#[cfg(test)]
#[path = "consent_cancel_tests.rs"]
mod cancellation_cache_tests;
