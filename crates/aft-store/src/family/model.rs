//! An exhaustive check of the publication and reclamation protocol.
//!
//! The model has the same steps as the code in `writer.rs`, `snapshot.rs`
//! and `lease.rs`, each taken as one atomic file operation, and explores
//! every interleaving of one writer and two readers, with readers crashing
//! at any point. The files a reclaimer judges liveness by are modelled as
//! files: each reader's owner file (present and locked while its process
//! lives, present and unlocked once it has ended, or removed), and its
//! lease record. A lease scan lists lease records and owner files in two
//! separate steps, as the code does, and removes the leases it judges dead.
//! It checks two properties in every reachable state:
//!
//! - a reader never reads a segment that has been removed;
//! - the newest manifest and every segment it names exist.
//!
//! Each control below removes one rule of the protocol, and the checker must
//! find a violation for it. That keeps the check honest: a model too weak to
//! fail would pass the real protocol for no reason.

use std::collections::{BTreeMap, BTreeSet, HashSet};

/// One way to break the protocol, for the controls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rule {
    /// The protocol as written.
    All,
    /// The horizon ignores the newest generation, so with no lease it has no
    /// bound and everything is reclaimed.
    NoNewestBound,
    /// The reclaimer does not check the leases again after writing the
    /// horizon.
    NoRescan,
    /// A reader does not check the horizon after taking its lease.
    NoReaderCheck,
    /// A reader takes its lease after checking the horizon instead of before.
    LeaseAfterCheck,
    /// A lease scan lists owner files before lease records, so a process
    /// that appears in between has its new lease judged dead and removed.
    OwnersFirst,
}

/// A reader's owner file in `leases/`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum OwnerFile {
    Absent,
    /// Locked by its live process.
    Locked,
    /// Its process has ended: the lock is free.
    Unlocked,
}

/// A lease record: the generation it names, and which of its reader's
/// leases it is (each lease is a new file).
type Lease = (u8, u8);

/// What a lease scan has listed so far.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Listing {
    /// Neither listing taken yet.
    None,
    /// Lease records listed, by reader.
    Leases(BTreeMap<usize, Lease>),
    /// Owner files listed: the readers whose owner file was present and
    /// locked.
    Owners(BTreeSet<usize>),
}

/// Which lease scan of a reclamation is running.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Scan {
    /// The first, before the horizon is computed.
    First,
    /// The second, after the horizon is written, with that horizon.
    Second(u8),
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Writer {
    Idle,
    /// The new segment is in `segments/`; the manifest is not written yet.
    Publishing {
        segment: u8,
    },
    /// Partway through a lease scan.
    Scanning {
        scan: Scan,
        listing: Listing,
    },
    /// The horizon is computed and not written yet.
    HorizonComputed {
        horizon: u8,
    },
    /// Leases checked again; the files are not removed yet. Holds the
    /// leases the second scan judged dead, removed after the files.
    Cleared {
        horizon: u8,
        dead: BTreeMap<usize, Lease>,
    },
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Reader {
    /// The process has not created its owner file yet.
    NoOwner,
    Start {
        tries: u8,
    },
    /// Read the newest manifest: its generation and the segments it names.
    ManifestRead {
        tries: u8,
        generation: u8,
        names: BTreeSet<u8>,
    },
    /// The lease record is written.
    Leased {
        tries: u8,
        generation: u8,
        names: BTreeSet<u8>,
    },
    /// (`LeaseAfterCheck` only) the horizon was checked; no lease yet.
    Checked {
        generation: u8,
        names: BTreeSet<u8>,
    },
    /// Passed the horizon check; reading the named segments.
    Reading {
        generation: u8,
        names: BTreeSet<u8>,
    },
    Done,
    Crashed,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct State {
    segments: BTreeSet<u8>,
    manifests: BTreeMap<u8, BTreeSet<u8>>,
    newest: u8,
    horizon: u8,
    next_segment: u8,
    writer: Writer,
    publications_left: u8,
    reclamations_left: u8,
    readers: Vec<Reader>,
    owners: Vec<OwnerFile>,
    leases: Vec<Option<Lease>>,
    /// How many leases each reader has taken, to name the next one.
    taken: Vec<u8>,
}

impl State {
    fn initial(readers: usize) -> Self {
        Self {
            segments: BTreeSet::from([1]),
            manifests: BTreeMap::from([(1, BTreeSet::from([1]))]),
            newest: 1,
            horizon: 0,
            next_segment: 2,
            writer: Writer::Idle,
            publications_left: 2,
            reclamations_left: 3,
            readers: vec![Reader::NoOwner; readers],
            owners: vec![OwnerFile::Absent; readers],
            leases: vec![None; readers],
            taken: vec![0; readers],
        }
    }

    fn list_leases(&self) -> BTreeMap<usize, Lease> {
        self.leases
            .iter()
            .enumerate()
            .filter_map(|(index, lease)| lease.map(|lease| (index, lease)))
            .collect()
    }

    fn list_owners(&self) -> BTreeSet<usize> {
        self.owners
            .iter()
            .enumerate()
            .filter(|(_, owner)| **owner == OwnerFile::Locked)
            .map(|(index, _)| index)
            .collect()
    }

    /// Removes the lease records in `dead` that are still the same files.
    fn remove_leases(&mut self, dead: &BTreeMap<usize, Lease>) {
        for (index, lease) in dead {
            if self.leases[*index] == Some(*lease) {
                self.leases[*index] = None;
            }
        }
    }

    fn take_lease(&mut self, index: usize, generation: u8) {
        self.leases[index] = Some((generation, self.taken[index]));
        self.taken[index] += 1;
    }
}

/// Splits listed leases into live ones and dead ones by the owners listed.
fn judge(
    leases: &BTreeMap<usize, Lease>,
    owners: &BTreeSet<usize>,
) -> (Vec<u8>, BTreeMap<usize, Lease>) {
    let mut live = Vec::new();
    let mut dead = BTreeMap::new();
    for (index, lease) in leases {
        if owners.contains(index) {
            live.push(lease.0);
        } else {
            dead.insert(*index, *lease);
        }
    }
    (live, dead)
}

/// The writer's possible steps from `state`.
fn writer_steps(state: &State, rule: Rule, next: &mut Vec<State>) {
    let leases_first = rule != Rule::OwnersFirst;
    match &state.writer {
        Writer::Idle => {
            if state.publications_left > 0 {
                let mut s = state.clone();
                s.publications_left -= 1;
                s.segments.insert(s.next_segment);
                s.writer = Writer::Publishing {
                    segment: s.next_segment,
                };
                s.next_segment += 1;
                next.push(s);
            }
            if state.reclamations_left > 0 {
                let mut s = state.clone();
                s.reclamations_left -= 1;
                s.writer = Writer::Scanning {
                    scan: Scan::First,
                    listing: Listing::None,
                };
                next.push(s);
            }
        }
        Writer::Publishing { segment } => {
            // Each publication replaces every segment, so a reclamation can
            // remove all of the previous generation's files.
            let mut s = state.clone();
            s.newest += 1;
            s.manifests.insert(s.newest, BTreeSet::from([*segment]));
            s.writer = Writer::Idle;
            next.push(s);
        }
        Writer::Scanning {
            scan,
            listing: Listing::None,
        } => {
            let mut s = state.clone();
            let listing = if leases_first {
                Listing::Leases(state.list_leases())
            } else {
                Listing::Owners(state.list_owners())
            };
            s.writer = Writer::Scanning {
                scan: *scan,
                listing,
            };
            next.push(s);
        }
        Writer::Scanning { scan, listing } => {
            let (leases, owners) = match listing {
                Listing::Leases(leases) => (leases.clone(), state.list_owners()),
                Listing::Owners(owners) => (state.list_leases(), owners.clone()),
                Listing::None => unreachable!("matched above"),
            };
            let (live, dead) = judge(&leases, &owners);
            let lowest = live.iter().copied().min();
            let mut s = state.clone();
            match scan {
                Scan::First => {
                    // Ended processes' files go first.
                    s.remove_leases(&dead);
                    let horizon = match rule {
                        Rule::NoNewestBound => lowest.unwrap_or(u8::MAX),
                        _ => lowest.map_or(state.newest, |lowest| lowest.min(state.newest)),
                    };
                    s.writer = Writer::HorizonComputed { horizon };
                }
                Scan::Second(horizon) => {
                    s.writer = if lowest.is_some_and(|lowest| lowest < *horizon) {
                        Writer::Idle
                    } else {
                        Writer::Cleared {
                            horizon: *horizon,
                            dead,
                        }
                    };
                }
            }
            next.push(s);
        }
        Writer::HorizonComputed { horizon } => {
            let mut s = state.clone();
            if *horizon < state.horizon {
                s.writer = Writer::Idle;
            } else {
                s.horizon = *horizon;
                s.writer = if rule == Rule::NoRescan {
                    Writer::Cleared {
                        horizon: *horizon,
                        dead: BTreeMap::new(),
                    }
                } else {
                    Writer::Scanning {
                        scan: Scan::Second(*horizon),
                        listing: Listing::None,
                    }
                };
            }
            next.push(s);
        }
        Writer::Cleared { horizon, dead } => {
            let mut s = state.clone();
            let keep = state
                .manifests
                .range(*horizon..)
                .flat_map(|(_, names)| names.iter().copied())
                .collect::<BTreeSet<_>>();
            s.segments.retain(|segment| keep.contains(segment));
            s.manifests.retain(|generation, _| generation >= horizon);
            s.remove_leases(dead);
            for (index, owner) in s.owners.iter_mut().enumerate() {
                // A dead owner file goes after its leases.
                if *owner == OwnerFile::Unlocked && s.leases[index].is_none() {
                    *owner = OwnerFile::Absent;
                }
            }
            s.writer = Writer::Idle;
            next.push(s);
        }
    }
}

/// Every state reachable in one step, or the violation a step runs into.
fn successors(state: &State, rule: Rule) -> Result<Vec<State>, String> {
    let mut next = Vec::new();
    writer_steps(state, rule, &mut next);

    for (index, reader) in state.readers.iter().enumerate() {
        let mut step = |reader: Reader, change: &dyn Fn(&mut State)| {
            let mut s = state.clone();
            s.readers[index] = reader;
            change(&mut s);
            next.push(s);
        };
        match reader {
            Reader::Done | Reader::Crashed => continue,
            Reader::NoOwner => step(Reader::Start { tries: 2 }, &|s| {
                s.owners[index] = OwnerFile::Locked;
            }),
            Reader::Start { tries } => {
                if *tries == 0 {
                    step(Reader::Done, &|_| {});
                } else {
                    // The newest manifest is never removed (checked for every
                    // state in `check`), so it can be read.
                    let names = state.manifests[&state.newest].clone();
                    step(
                        Reader::ManifestRead {
                            tries: tries - 1,
                            generation: state.newest,
                            names,
                        },
                        &|_| {},
                    );
                }
            }
            Reader::ManifestRead {
                tries,
                generation,
                names,
            } => {
                if rule == Rule::LeaseAfterCheck {
                    if state.horizon > *generation {
                        step(Reader::Start { tries: *tries }, &|_| {});
                    } else {
                        step(
                            Reader::Checked {
                                generation: *generation,
                                names: names.clone(),
                            },
                            &|_| {},
                        );
                    }
                } else {
                    let generation = *generation;
                    step(
                        Reader::Leased {
                            tries: *tries,
                            generation,
                            names: names.clone(),
                        },
                        &|s| s.take_lease(index, generation),
                    );
                }
            }
            Reader::Checked { generation, names } => {
                let generation = *generation;
                step(
                    Reader::Reading {
                        generation,
                        names: names.clone(),
                    },
                    &|s| s.take_lease(index, generation),
                );
            }
            Reader::Leased {
                tries,
                generation,
                names,
            } => {
                if rule != Rule::NoReaderCheck && state.horizon > *generation {
                    // Dropping the snapshot removes its lease record.
                    step(Reader::Start { tries: *tries }, &|s| {
                        s.leases[index] = None;
                    });
                } else {
                    step(
                        Reader::Reading {
                            generation: *generation,
                            names: names.clone(),
                        },
                        &|_| {},
                    );
                }
            }
            Reader::Reading { generation, names } => {
                if let Some(missing) = names.iter().find(|name| !state.segments.contains(name)) {
                    return Err(format!(
                        "reader {index} read segment {missing} of generation {generation} after it was removed"
                    ));
                }
                step(Reader::Done, &|s| s.leases[index] = None);
            }
        }
        if !matches!(reader, Reader::Done | Reader::Crashed) {
            // The process ends: its owner file's lock is released, and its
            // lease record stays until a reclaimer removes it.
            let mut s = state.clone();
            s.readers[index] = Reader::Crashed;
            if s.owners[index] == OwnerFile::Locked {
                s.owners[index] = OwnerFile::Unlocked;
            }
            next.push(s);
        }
    }
    Ok(next)
}

/// Explores every reachable state. Returns the number of states, or the
/// first violation.
fn check(rule: Rule, readers: usize) -> Result<usize, String> {
    let initial = State::initial(readers);
    let mut seen = HashSet::from([initial.clone()]);
    let mut stack = vec![initial];
    while let Some(state) = stack.pop() {
        let Some(names) = state.manifests.get(&state.newest) else {
            return Err(format!(
                "the newest manifest, {}, was removed",
                state.newest
            ));
        };
        if let Some(missing) = names.iter().find(|name| !state.segments.contains(name)) {
            return Err(format!(
                "segment {missing} of the newest generation {} was removed",
                state.newest
            ));
        }
        for next in successors(&state, rule)? {
            if seen.insert(next.clone()) {
                stack.push(next);
            }
        }
    }
    Ok(seen.len())
}

#[test]
fn the_protocol_holds_in_every_interleaving() {
    for readers in 1..=2 {
        let states = check(Rule::All, readers).unwrap();
        println!("{readers} readers: {states} states");
        assert!(
            states > 1_000,
            "{readers} readers: only {states} states explored"
        );
    }
}

#[test]
fn removing_any_rule_breaks_the_protocol() {
    for rule in [
        Rule::NoNewestBound,
        Rule::NoRescan,
        Rule::NoReaderCheck,
        Rule::LeaseAfterCheck,
        Rule::OwnersFirst,
    ] {
        let found = check(rule, 2);
        println!("{rule:?}: {found:?}");
        assert!(
            found.is_err(),
            "{rule:?} found no violation in {found:?} states"
        );
    }
}
