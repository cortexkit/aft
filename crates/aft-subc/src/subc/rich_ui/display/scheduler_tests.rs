use super::*;
use std::time::Duration;

fn identity(call: usize) -> Identity {
    Identity {
        runner: Principal::Reserved {
            module_id: "runner".into(),
        },
        call_key: format!("call:{call}"),
        schema_pin: "pin:1".into(),
    }
}

fn admit(outbox: &mut Outbox, call: usize, kind: DisplayKind, now: Instant) {
    assert!(outbox.admit(
        identity(call),
        kind,
        json!({"durationMs":1,"outcome":"success"}),
        &outbox::ceilings(),
        now,
    ));
}

#[tokio::test(start_paused = true)]
async fn pipeline_review_display_burst_drains_one_coalesced_wake() {
    let mut outbox = Outbox::new();
    let start = tokio::time::Instant::now();
    for call in 0..50 {
        for kind in [DisplayKind::Record, DisplayKind::Detail] {
            admit(&mut outbox, call, kind, start.into_std());
        }
    }
    let outbox = Mutex::new(outbox);
    let (wake, mut receiver) = tokio::sync::mpsc::channel(1);
    wake.try_send(()).unwrap();
    for _ in 0..100 {
        assert!(next_display_entry(&outbox, &mut receiver).await.is_some());
        // Model the same 10 ms transport cost per record on both schedulers.
        tokio::time::advance(Duration::from_millis(10)).await;
    }
    let elapsed = start.elapsed();
    eprintln!(
        "100 display records: {elapsed:?}, {:.2} records/s",
        100.0 / elapsed.as_secs_f64()
    );
    assert_eq!(elapsed, Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn pipeline_review_display_scheduler_idles_and_respects_retry_deadlines() {
    let (wake, mut receiver) = tokio::sync::mpsc::channel(1);
    let start = tokio::time::Instant::now();
    let outbox = Mutex::new(Outbox::new());
    assert_eq!(outbox.lock().unwrap().next_wake_at(), None);
    admit(
        &mut outbox.lock().unwrap(),
        0,
        DisplayKind::Record,
        start.into_std(),
    );
    wake.try_send(()).unwrap();
    let entry = next_display_entry(&outbox, &mut receiver).await.unwrap();
    outbox.lock().unwrap().retry(entry, start.into_std());
    let deadline = outbox.lock().unwrap().next_wake_at().unwrap();
    let mut retry = std::pin::pin!(next_display_entry(&outbox, &mut receiver));
    assert!(futures_util::poll!(&mut retry).is_pending());
    assert_eq!(tokio::time::Instant::now(), start);
    tokio::time::advance(deadline.duration_since(start.into_std())).await;
    let entry = retry.await.unwrap();
    assert_eq!(entry.attempts, 1);
    assert!(tokio::time::Instant::now().into_std() >= deadline);
    assert_eq!(outbox.lock().unwrap().next_wake_at(), None);
}
