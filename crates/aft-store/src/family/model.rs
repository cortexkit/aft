//! An exhaustive check of the publication and reclamation protocol.
//!
//! The model has the same steps as the code in `writer.rs`, `snapshot.rs`
//! and `lease.rs`, each taken as one atomic file operation, and explores
//! every interleaving of one writer and two readers, with readers crashing
//! at any point. It checks two properties in every reachable state:
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
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Writer {
    Idle,
    /// The new segment is in `segments/`; the manifest is not written yet.
    Publishing {
        segment: u8,
    },
    /// The horizon is computed and not written yet.
    HorizonComputed {
        horizon: u8,
    },
    /// The horizon is written; the leases are not checked again yet.
    HorizonWritten {
        horizon: u8,
    },
    /// Leases checked again; the files are not removed yet.
    Cleared {
        horizon: u8,
    },
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Reader {
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
        tries: u8,
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
            readers: vec![Reader::Start { tries: 2 }; readers],
        }
    }

    /// Generations live readers hold leases on. A crashed reader's lease
    /// belongs to an ended process: the reclaimer ignores and removes it.
    fn live_leases(&self) -> Vec<u8> {
        self.readers
            .iter()
            .filter_map(|reader| match reader {
                Reader::Leased { generation, .. } | Reader::Reading { generation, .. } => {
                    Some(*generation)
                }
                _ => None,
            })
            .collect()
    }
}

/// Every state reachable in one step, or the violation a step runs into.
fn successors(state: &State, rule: Rule) -> Result<Vec<State>, String> {
    let mut next = Vec::new();

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
                let lowest = state.live_leases().into_iter().min();
                let horizon = match rule {
                    Rule::NoNewestBound => lowest.unwrap_or(u8::MAX),
                    _ => lowest.map_or(state.newest, |lowest| lowest.min(state.newest)),
                };
                s.writer = Writer::HorizonComputed { horizon };
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
        Writer::HorizonComputed { horizon } => {
            let mut s = state.clone();
            if *horizon < state.horizon {
                s.writer = Writer::Idle;
            } else {
                s.horizon = *horizon;
                s.writer = Writer::HorizonWritten { horizon: *horizon };
            }
            next.push(s);
        }
        Writer::HorizonWritten { horizon } => {
            let mut s = state.clone();
            let raced =
                rule != Rule::NoRescan && state.live_leases().iter().any(|lease| lease < horizon);
            s.writer = if raced {
                Writer::Idle
            } else {
                Writer::Cleared { horizon: *horizon }
            };
            next.push(s);
        }
        Writer::Cleared { horizon } => {
            let mut s = state.clone();
            let keep = state
                .manifests
                .range(*horizon..)
                .flat_map(|(_, names)| names.iter().copied())
                .collect::<BTreeSet<_>>();
            s.segments.retain(|segment| keep.contains(segment));
            s.manifests.retain(|generation, _| generation >= horizon);
            s.writer = Writer::Idle;
            next.push(s);
        }
    }

    for (index, reader) in state.readers.iter().enumerate() {
        let mut step = |reader: Reader| {
            let mut s = state.clone();
            s.readers[index] = reader;
            next.push(s);
        };
        match reader {
            Reader::Done | Reader::Crashed => continue,
            Reader::Start { tries } => {
                if *tries == 0 {
                    step(Reader::Done);
                } else {
                    // The newest manifest is never removed (checked for every
                    // state in `check`), so it can be read.
                    let names = state.manifests[&state.newest].clone();
                    step(Reader::ManifestRead {
                        tries: tries - 1,
                        generation: state.newest,
                        names,
                    });
                }
            }
            Reader::ManifestRead {
                tries,
                generation,
                names,
            } => {
                if rule == Rule::LeaseAfterCheck {
                    if state.horizon > *generation {
                        step(Reader::Start { tries: *tries });
                    } else {
                        step(Reader::Checked {
                            tries: *tries,
                            generation: *generation,
                            names: names.clone(),
                        });
                    }
                } else {
                    step(Reader::Leased {
                        tries: *tries,
                        generation: *generation,
                        names: names.clone(),
                    });
                }
            }
            Reader::Checked {
                generation, names, ..
            } => step(Reader::Reading {
                generation: *generation,
                names: names.clone(),
            }),
            Reader::Leased {
                tries,
                generation,
                names,
            } => {
                if rule != Rule::NoReaderCheck && state.horizon > *generation {
                    step(Reader::Start { tries: *tries });
                } else {
                    step(Reader::Reading {
                        generation: *generation,
                        names: names.clone(),
                    });
                }
            }
            Reader::Reading { generation, names } => {
                if let Some(missing) = names.iter().find(|name| !state.segments.contains(name)) {
                    return Err(format!(
                        "reader {index} read segment {missing} of generation {generation} after it was removed"
                    ));
                }
                step(Reader::Done);
            }
        }
        if !matches!(reader, Reader::Done | Reader::Crashed) {
            let mut s = state.clone();
            s.readers[index] = Reader::Crashed;
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
    ] {
        let found = check(rule, 2);
        println!("{rule:?}: {found:?}");
        assert!(
            found.is_err(),
            "{rule:?} found no violation in {found:?} states"
        );
    }
}
