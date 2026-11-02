use std::cell::Cell;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Measurements {
    pub binds: usize,
    pub reconciles: usize,
    pub bind_time: Duration,
    pub reconcile_time: Duration,
}

thread_local! {
    static MEASUREMENTS: Cell<Measurements> = Cell::default();
}

pub(crate) fn take_measurements() -> Measurements {
    MEASUREMENTS.with(|work| work.take())
}

pub(crate) struct Measurement {
    start: Instant,
    bind: bool,
}

pub(crate) fn measure_bind() -> Measurement {
    Measurement {
        start: Instant::now(),
        bind: true,
    }
}

pub(crate) fn measure_reconcile() -> Measurement {
    Measurement {
        start: Instant::now(),
        bind: false,
    }
}

impl Drop for Measurement {
    fn drop(&mut self) {
        MEASUREMENTS.with(|work| {
            let mut counts = work.get();
            if self.bind {
                counts.binds += 1;
                counts.bind_time += self.start.elapsed();
            } else {
                counts.reconciles += 1;
                counts.reconcile_time += self.start.elapsed();
            }
            work.set(counts);
        });
    }
}
