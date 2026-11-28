//! The watch that ends a captured bot write when its authority is revoked or
//! its scope ends while the post is in flight.
//!
//! This process's own revocations and scope ends are kept in memory and are
//! checked every tick. The durable check reads `aft.db` (it opens a
//! connection), so it runs on the blocking pool every
//! [`DURABLE_EVERY_TICKS`] ticks, or sooner when this process commits an end.

use std::time::Duration;
use tokio::sync::watch;

use crate::gh_shim_speech::Refusal;

/// How often the in-memory pending sets are checked.
pub(super) const TICK: Duration = Duration::from_millis(100);
/// Without a local scope end, check durability once a second after the first tick.
pub(super) const DURABLE_EVERY_TICKS: u32 = 10;

/// Run until a check refuses. The watch coalesces commits since the last read;
/// the durable check determines whether an end applies to the captured write.
pub(super) async fn until_refused<M, D>(
    in_memory: M,
    durable: D,
    mut scope_ends: watch::Receiver<()>,
) -> Refusal
where
    M: Fn() -> Result<(), Refusal>,
    D: Fn() -> Result<(), Refusal> + Clone + Send + 'static,
{
    let mut tick: u32 = 0;
    let mut scope_end = false;
    loop {
        if let Err(refusal) = in_memory() {
            return refusal;
        }
        if scope_end || tick % DURABLE_EVERY_TICKS == 0 {
            scope_ends.borrow_and_update();
            let check = durable.clone();
            let result = match tokio::task::spawn_blocking(check).await {
                Ok(result) => result,
                // The blocking pool could not run it (a panic or shutdown):
                // run the check here rather than skip it.
                Err(_) => durable(),
            };
            if let Err(refusal) = result {
                return refusal;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(TICK) => {
                tick = tick.wrapping_add(1);
                scope_end = false;
            }
            Ok(()) = scope_ends.changed() => scope_end = true,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;
    use crate::gh_shim_speech::Code;

    #[tokio::test(start_paused = true)]
    async fn two_seconds_of_ticks_read_the_database_at_most_three_times() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let durable = Arc::new(AtomicUsize::new(0));
        let (_ends, changes) = watch::channel(());
        // The post ends on the 22nd tick: 2.1 s of watching at 100 ms a tick.
        until_refused(
            {
                let ticks = Arc::clone(&ticks);
                move || {
                    if ticks.fetch_add(1, Ordering::SeqCst) == 21 {
                        Err(Refusal::new(Code::AuthorityUnavailable, "post_finished"))
                    } else {
                        Ok(())
                    }
                }
            },
            {
                let durable = Arc::clone(&durable);
                move || {
                    durable.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
            changes,
        )
        .await;
        assert_eq!(ticks.load(Ordering::SeqCst), 22);
        // Ticks 0, 10 and 20; checking on every tick made it 21.
        assert_eq!(durable.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn an_in_memory_revocation_ends_the_watch_on_its_tick() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let (_ends, changes) = watch::channel(());
        let refusal = until_refused(
            {
                let ticks = Arc::clone(&ticks);
                move || {
                    if ticks.fetch_add(1, Ordering::SeqCst) == 3 {
                        Err(Refusal::new(
                            Code::AuthorityUnavailable,
                            "revocation_pending",
                        ))
                    } else {
                        Ok(())
                    }
                }
            },
            || Ok(()),
            changes,
        )
        .await;
        assert_eq!(
            refusal,
            Refusal::new(Code::AuthorityUnavailable, "revocation_pending")
        );
        assert_eq!(ticks.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn committed_scope_ends_between_memory_checks_wake_one_durable_read() {
        let (ends, changes) = watch::channel(());
        let persisted = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicUsize::new(0));
        let ticks = AtomicUsize::new(0);
        let refusal = until_refused(
            || {
                if ticks.fetch_add(1, Ordering::SeqCst) == 1 {
                    // Model a burst committed after the memory check saw no
                    // pending end. Only the durable check will see the ends.
                    persisted.store(true, Ordering::SeqCst);
                    ends.send_replace(());
                    ends.send_replace(());
                }
                Ok(())
            },
            {
                let persisted = Arc::clone(&persisted);
                let reads = Arc::clone(&reads);
                move || {
                    reads.fetch_add(1, Ordering::SeqCst);
                    if persisted.load(Ordering::SeqCst) {
                        Err(Refusal::new(Code::ScopeEnded, "ended"))
                    } else {
                        Ok(())
                    }
                }
            },
            changes,
        )
        .await;
        assert_eq!(refusal, Refusal::new(Code::ScopeEnded, "ended"));
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert_eq!(ticks.load(Ordering::SeqCst), 3, "must not wait ten ticks");
    }
}
