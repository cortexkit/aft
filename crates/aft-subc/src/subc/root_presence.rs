use super::{Arc, AtomicBool, HashMap, Ordering, Path, ProjectRootId, RootMeta};

#[derive(Clone, Copy, Debug)]
pub(super) enum Observation {
    Present,
    Absent,
    Unknown,
}

/// Completed directory checks. A failed stat is evidence of deletion only
/// if the filesystem explicitly reports that the path does not exist.
#[derive(Clone, Debug, Default)]
pub(super) struct RootPresence {
    /// Increases with every probe; zero means no probe has finished.
    pub(super) generation: u64,
    pub(super) observations: HashMap<ProjectRootId, Observation>,
}

impl RootPresence {
    pub(super) fn probe<'a>(
        generation: u64,
        roots: impl IntoIterator<Item = &'a ProjectRootId>,
    ) -> Self {
        Self::probe_with(generation, roots, |path| path.metadata().map(drop))
    }

    pub(super) fn probe_with<'a>(
        generation: u64,
        roots: impl IntoIterator<Item = &'a ProjectRootId>,
        mut stat: impl FnMut(&Path) -> std::io::Result<()>,
    ) -> Self {
        let observations = roots
            .into_iter()
            .map(|root| {
                let observed = match stat(root.as_path()) {
                    Ok(()) => Observation::Present,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        Observation::Absent
                    }
                    Err(_) => Observation::Unknown,
                };
                (root.clone(), observed)
            })
            .collect();
        Self {
            generation,
            observations,
        }
    }

    /// A transient miss (including atomic directory replacement) does not
    /// justify reclaim. Unknown checks interrupt the proof; reusing the same
    /// completed probe on another sweep does not count as a new check.
    pub(super) fn update_absence(&self, root: &ProjectRootId, meta: &mut RootMeta) -> bool {
        if self.generation > meta.presence_generation_seen {
            if let Some(observation) = self.observations.get(root) {
                meta.presence_generation_seen = self.generation;
                meta.consecutive_missing_sweeps = match observation {
                    Observation::Absent => meta.consecutive_missing_sweeps.saturating_add(1),
                    Observation::Present | Observation::Unknown => 0,
                };
            }
        }
        meta.consecutive_missing_sweeps > 0
    }
}

/// Runs directory checks off the frame loop, at most one at a time and at
/// most once a second.
pub(super) struct RootPresenceProbe {
    latest: Arc<parking_lot::Mutex<Arc<RootPresence>>>,
    in_flight: Arc<AtomicBool>,
    pub(super) next_generation: u64,
    seen_generation: u64,
    next_at: tokio::time::Instant,
}

impl RootPresenceProbe {
    pub(super) fn new() -> Self {
        Self {
            latest: Arc::default(),
            in_flight: Arc::new(AtomicBool::new(false)),
            next_generation: 0,
            seen_generation: 0,
            next_at: tokio::time::Instant::now(),
        }
    }

    /// Publication in progress reads as no new observation, never a miss.
    pub(super) fn latest(&mut self) -> Arc<RootPresence> {
        let Some(presence) = self.latest.try_lock() else {
            return Arc::default();
        };
        self.seen_generation = presence.generation;
        Arc::clone(&presence)
    }

    /// A probe stuck on a hung filesystem only delays deletion detection.
    /// Roots whose idle artifacts are already evicted are not probed.
    pub(super) fn request(&mut self, live_roots: &HashMap<ProjectRootId, RootMeta>) {
        let now = tokio::time::Instant::now();
        if now < self.next_at || self.in_flight.load(Ordering::Acquire) {
            return;
        }
        self.next_at = now + std::time::Duration::from_secs(1);
        let roots: Vec<_> = live_roots
            .iter()
            .filter(|(_, meta)| !meta.idle_artifacts_evicted)
            .map(|(root, _)| root.clone())
            .collect();
        // Do not overwrite a finished sample before it has been read: an
        // unknown sample between two misses must interrupt deletion proof.
        if roots.is_empty()
            || self.seen_generation != self.next_generation
            || self.in_flight.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let generation = self.next_generation + 1;
        let latest = Arc::clone(&self.latest);
        let in_flight = Arc::clone(&self.in_flight);
        let spawned = std::thread::Builder::new()
            .name("aft-root-presence".to_owned())
            .spawn(move || {
                let presence = RootPresence::probe(generation, &roots);
                *latest.lock() = Arc::new(presence);
                in_flight.store(false, Ordering::Release);
            });
        if spawned.is_ok() {
            self.next_generation = generation;
        } else {
            self.in_flight.store(false, Ordering::Release);
        }
    }
}
