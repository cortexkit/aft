//! Wall-time attribution for view materialization.

use std::cell::RefCell;
use std::time::Instant;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PhaseTimings {
    pub(crate) load_bindings_select_ms: u128,
    pub(crate) delete_rows_ms: u128,
    pub(crate) owned_blob_decode_and_insert_ms: u128,
    pub(crate) join_load_payloads_ms: u128,
    pub(crate) join_decode_bind_index_entries_ms: u128,
    pub(crate) join_index_and_surface_replay_ms: u128,
    pub(crate) join_decode_resolved_callers_ms: u128,
    pub(crate) join_resolve_and_record_ms: u128,
    pub(crate) join_dependency_union_ms: u128,
    pub(crate) selected_join_ms: u128,
    pub(crate) write_bindings_ms: u128,
    pub(crate) emit_refs_edges_ms: u128,
    pub(crate) commit_ms: u128,
    pub(crate) cleanup_memory_ms: u128,
    pub(crate) cleanup_connections_ms: u128,
    pub(crate) writes: WritePhaseTimings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WritePhase {
    DeleteRows,
    SelectedJoin,
    Files,
    Nodes,
    FileSurfaces,
    FileDependencies,
    Bindings,
    Refs,
    Edges,
    IndexMaintenance,
}

impl WritePhase {
    pub(crate) const ALL: [Self; 10] = [
        Self::DeleteRows,
        Self::SelectedJoin,
        Self::Files,
        Self::Nodes,
        Self::FileSurfaces,
        Self::FileDependencies,
        Self::Bindings,
        Self::Refs,
        Self::Edges,
        Self::IndexMaintenance,
    ];

    #[cfg(test)]
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::DeleteRows => "delete_rows",
            Self::SelectedJoin => "selected_join",
            Self::Files => "emit_files",
            Self::Nodes => "emit_nodes",
            Self::FileSurfaces => "emit_view_file_surfaces",
            Self::FileDependencies => "emit_file_dependencies",
            Self::Bindings => "emit_view_bindings",
            Self::Refs => "emit_refs",
            Self::Edges => "emit_edges",
            Self::IndexMaintenance => "index_maintenance",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PhaseMeasurement {
    pub(crate) wall_ns: u128,
    pub(crate) page_writes: u64,
    pub(crate) samples: u64,
}

impl PhaseMeasurement {
    pub(crate) fn observed(wall_ns: u128, page_writes: u64) -> Self {
        Self {
            wall_ns,
            page_writes,
            samples: 1,
        }
    }

    fn add(&mut self, other: Self) {
        self.wall_ns += other.wall_ns;
        self.page_writes += other.page_writes;
        self.samples += other.samples;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WritePhaseTimings {
    phases: [PhaseMeasurement; WritePhase::ALL.len()],
}

impl Default for WritePhaseTimings {
    fn default() -> Self {
        Self {
            phases: [PhaseMeasurement::default(); WritePhase::ALL.len()],
        }
    }
}

impl WritePhaseTimings {
    pub(crate) fn get(&self, phase: WritePhase) -> PhaseMeasurement {
        self.phases[phase.index()]
    }

    fn record(&mut self, phase: WritePhase, measurement: PhaseMeasurement) {
        self.phases[phase.index()].add(measurement);
    }
}

#[cfg(test)]
pub(crate) fn offline_phase_table(
    clone: PhaseMeasurement,
    timings: &PhaseTimings,
    checkpoint: PhaseMeasurement,
) -> std::result::Result<String, &'static str> {
    let mut rows = Vec::with_capacity(WritePhase::ALL.len() + 2);
    rows.push(("clone", clone));
    for phase in [
        WritePhase::SelectedJoin,
        WritePhase::DeleteRows,
        WritePhase::Files,
        WritePhase::Nodes,
        WritePhase::FileSurfaces,
        WritePhase::FileDependencies,
        WritePhase::Bindings,
        WritePhase::Refs,
        WritePhase::Edges,
        WritePhase::IndexMaintenance,
    ] {
        rows.push((phase.label(), timings.writes.get(phase)));
    }
    rows.push(("checkpoint", checkpoint));
    if let Some((label, _)) = rows
        .iter()
        .find(|(_, measurement)| measurement.samples == 0)
    {
        return Err(label);
    }
    let mut table = String::from("| phase | wall_ms | sqlite_page_writes |\n|---|---:|---:|\n");
    for (label, measurement) in rows {
        use std::fmt::Write as _;
        let _ = writeln!(
            table,
            "| {label} | {:.3} | {} |",
            measurement.wall_ns as f64 / 1_000_000.0,
            measurement.page_writes
        );
    }
    Ok(table)
}

thread_local! {
    static ACTIVE_MATERIALIZATION: RefCell<Option<PhaseTimings>> = const { RefCell::new(None) };
}

pub(crate) struct PhaseTimer {
    start: Instant,
    emit_stderr: bool,
    collect_writes: bool,
    prefix: &'static str,
}

impl PhaseTimer {
    pub(crate) fn new(prefix: &'static str) -> Self {
        if prefix != "join" {
            ACTIVE_MATERIALIZATION.with(|active| active.replace(Some(PhaseTimings::default())));
        }
        let emit_stderr = std::env::var_os("AFT_VIEW_PROFILE").is_some();
        Self {
            start: Instant::now(),
            emit_stderr,
            collect_writes: cfg!(test) || emit_stderr,
            prefix,
        }
    }

    pub(crate) fn measure<T>(
        &mut self,
        phase: WritePhase,
        connection: &rusqlite::Connection,
        operation: impl FnOnce() -> T,
    ) -> T {
        if !self.collect_writes {
            return operation();
        }
        let probe = PhaseProbe::start(connection);
        let result = operation();
        self.record(phase, probe.finish(connection));
        result
    }

    pub(crate) fn record(&mut self, phase: WritePhase, measurement: PhaseMeasurement) {
        if !self.collect_writes {
            return;
        }
        ACTIVE_MATERIALIZATION.with(|active| {
            if let Some(timings) = active.borrow_mut().as_mut() {
                timings.writes.record(phase, measurement);
            }
        });
    }

    pub(crate) fn observe_empty(&mut self, phase: WritePhase) {
        if !self.collect_writes {
            return;
        }
        ACTIVE_MATERIALIZATION.with(|active| {
            if let Some(timings) = active.borrow_mut().as_mut() {
                if timings.writes.get(phase).samples == 0 {
                    timings
                        .writes
                        .record(phase, PhaseMeasurement::observed(0, 0));
                }
            }
        });
    }

    pub(crate) fn finish(&mut self, phase: &str) {
        let elapsed = self.start.elapsed();
        let elapsed_ms = elapsed.as_millis();
        ACTIVE_MATERIALIZATION.with(|active| {
            let mut active = active.borrow_mut();
            let Some(timings) = active.as_mut() else {
                return;
            };
            match (self.prefix, phase) {
                (_, "load_bindings_select") => timings.load_bindings_select_ms = elapsed_ms,
                (_, "delete_rows") => timings.delete_rows_ms = elapsed_ms,
                (_, "owned_blob_decode_and_insert") => {
                    timings.owned_blob_decode_and_insert_ms = elapsed_ms;
                }
                ("join", "load_payloads") => timings.join_load_payloads_ms = elapsed_ms,
                ("join", "decode_bind_index_entries") => {
                    timings.join_decode_bind_index_entries_ms = elapsed_ms;
                }
                ("join", "index_and_surface_replay") => {
                    timings.join_index_and_surface_replay_ms = elapsed_ms;
                }
                ("join", "decode_resolved_callers") => {
                    timings.join_decode_resolved_callers_ms = elapsed_ms;
                }
                ("join", "resolve_and_record") => {
                    timings.join_resolve_and_record_ms = elapsed_ms;
                }
                ("join", "dependency_union") => {
                    timings.join_dependency_union_ms = elapsed_ms;
                }
                (_, "selected_join") => timings.selected_join_ms = elapsed_ms,
                (_, "write_bindings") => timings.write_bindings_ms = elapsed_ms,
                (_, "emit_refs_edges") => timings.emit_refs_edges_ms = elapsed_ms,
                (_, "commit") => timings.commit_ms = elapsed_ms,
                (_, "cleanup_memory") => timings.cleanup_memory_ms = elapsed_ms,
                (_, "cleanup_connections") => timings.cleanup_connections_ms = elapsed_ms,
                _ => {}
            }
        });
        if self.emit_stderr {
            eprintln!(
                "view_profile {}.{phase} ms={:.3}",
                self.prefix,
                elapsed.as_secs_f64() * 1000.0
            );
        }
        self.start = Instant::now();
    }

    pub(crate) fn into_timings(self) -> PhaseTimings {
        ACTIVE_MATERIALIZATION
            .with(|active| active.borrow_mut().take())
            .unwrap_or_default()
    }
}

pub(crate) struct PhaseProbe {
    started: Instant,
    cache_writes_before: Option<u64>,
}

impl PhaseProbe {
    pub(crate) fn start(connection: &rusqlite::Connection) -> Self {
        Self {
            started: Instant::now(),
            cache_writes_before: cache_writes(connection),
        }
    }

    pub(crate) fn finish(self, connection: &rusqlite::Connection) -> PhaseMeasurement {
        PhaseMeasurement {
            wall_ns: self.started.elapsed().as_nanos(),
            page_writes: self
                .cache_writes_before
                .zip(cache_writes(connection))
                .map_or(0, |(before, after)| after.saturating_sub(before)),
            samples: 1,
        }
    }
}

fn cache_writes(connection: &rusqlite::Connection) -> Option<u64> {
    let mut current = 0;
    let mut highwater = 0;
    let result = unsafe {
        rusqlite::ffi::sqlite3_db_status(
            connection.handle(),
            rusqlite::ffi::SQLITE_DBSTATUS_CACHE_WRITE,
            &mut current,
            &mut highwater,
            0,
        )
    };
    (result == rusqlite::ffi::SQLITE_OK).then_some(current.max(0) as u64)
}

/// Optional diagnostics only; never checkpoint or change the writer's durability.
pub(super) struct WriteProbe {
    process_before: Option<(u64, u64)>,
    wal_before: u64,
}

impl WriteProbe {
    pub(super) fn start(path: &std::path::Path) -> Option<Self> {
        std::env::var_os("AFT_VIEW_PROFILE")?;
        Some(Self {
            process_before: process_writes(),
            wal_before: wal_len(path),
        })
    }

    pub(super) fn finish(&self, connection: &rusqlite::Connection, path: &std::path::Path) {
        // This connection is exclusively owned by the materializer. Reading a
        // status counter does not flush the pager or reset its accounting.
        let cache_writes = cache_writes(connection);
        let page_size = connection
            .query_row("PRAGMA page_size", [], |row| row.get::<_, u64>(0))
            .ok();
        let process_delta = self
            .process_before
            .zip(process_writes())
            .map(|(before, after)| {
                (
                    after.0.saturating_sub(before.0),
                    after.1.saturating_sub(before.1),
                )
            });
        eprintln!("view_profile derived_writes db={} cache_write_pages={cache_writes:?} page_size={page_size:?} wal_before={} wal_after={} process_physical_logical_delta={process_delta:?}", path.display(), self.wal_before, wal_len(path));
    }
}

fn wal_len(path: &std::path::Path) -> u64 {
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    std::fs::metadata(wal).map_or(0, |metadata| metadata.len())
}

#[cfg(target_os = "macos")]
fn process_writes() -> Option<(u64, u64)> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    let result = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            usage.as_mut_ptr().cast(),
        )
    };
    if result != 0 {
        return None;
    }
    let usage = unsafe { usage.assume_init() };
    Some((usage.ri_diskio_byteswritten, usage.ri_logical_writes))
}

#[cfg(not(target_os = "macos"))]
fn process_writes() -> Option<(u64, u64)> {
    None
}
