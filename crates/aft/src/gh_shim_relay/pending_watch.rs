//! The watch that ends a captured bot write when its authority is revoked or
//! its scope ends while the post is in flight.
//!
//! This process's own revocations and scope ends are kept in memory and are
//! checked every tick. The durable check reads `aft.db` (it opens a
//! connection), so it runs at most once per [`DURABLE_EVERY_TICKS`] ticks and
//! on the blocking pool, never on a runtime worker thread.

use std::time::Duration;

use crate::gh_shim_speech::Refusal;

/// How often the in-memory pending sets are checked.
pub(super) const TICK: Duration = Duration::from_millis(100);
/// The durable check runs on the first tick and then every this many ticks,
/// so at most once a second.
pub(super) const DURABLE_EVERY_TICKS: u32 = 10;

/// Run until a check refuses, and return that refusal. `durable` runs on the
/// blocking pool.
pub(super) async fn until_refused<M, D>(in_memory: M, durable: D) -> Refusal
where
    M: Fn() -> Result<(), Refusal>,
    D: Fn() -> Result<(), Refusal> + Clone + Send + 'static,
{
    let mut tick: u32 = 0;
    loop {
        if let Err(refusal) = in_memory() {
            return refusal;
        }
        if tick % DURABLE_EVERY_TICKS == 0 {
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
        tick = tick.wrapping_add(1);
        tokio::time::sleep(TICK).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;
    use crate::gh_shim_speech::Code;

    #[tokio::test(start_paused = true)]
    async fn a_two_second_post_reads_the_database_at_most_three_times() {
        let in_memory = Arc::new(AtomicUsize::new(0));
        let durable = Arc::new(AtomicUsize::new(0));
        let watch = until_refused(
            {
                let in_memory = Arc::clone(&in_memory);
                move || {
                    in_memory.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
            {
                let durable = Arc::clone(&durable);
                move || {
                    durable.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
        );
        // The post itself takes two seconds on the paused test clock.
        assert!(tokio::time::timeout(Duration::from_secs(2), watch)
            .await
            .is_err());
        assert_eq!(in_memory.load(Ordering::SeqCst), 21);
        assert_eq!(durable.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn an_in_memory_revocation_ends_the_watch_on_its_tick() {
        let ticks = Arc::new(AtomicUsize::new(0));
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
        )
        .await;
        assert_eq!(
            refusal,
            Refusal::new(Code::AuthorityUnavailable, "revocation_pending")
        );
        assert_eq!(ticks.load(Ordering::SeqCst), 4);
    }
}
