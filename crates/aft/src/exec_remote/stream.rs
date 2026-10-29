use super::*;
use std::{collections::BTreeMap, io};
use types::{
    Accepted, AttachRequest, Output, OutputStream, ReturnChunk, ReturnManifest, Started,
    StreamRecord, Uuid,
};

/// Durable resume cursor. `last_seq` is the last contiguous record committed
/// by the sink, not the largest sequence observed on the connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumePoint {
    pub job_id: Uuid,
    pub last_seq: Option<u64>,
    pub gap_recovery: Option<GapRecovery>,
    /// The seq of the last output (or start) record committed. An expired
    /// replay's missing seqs are counted from it.
    pub last_output_seq: Option<u64>,
}

/// An executor terminal received while output has a gap. Persist its outcome,
/// buffered later records and retries without contiguous cursor progress. Keep
/// it separate from a committed terminal until output is drained, recovering
/// missing records for up to three attaches before declaring only output lost.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GapRecovery {
    pub from_seq: u64,
    pub reattaches: u8,
    pub terminal: TerminalRecord,
    pending: BTreeMap<u64, Pending>,
}

// The original terminal starts recovery; three attaches without cursor progress
// are enough to establish missing output without discarding a transient gap.
const MAX_GAP_REATTACHES: u8 = 3;

impl ResumePoint {
    pub fn attach_request(&self) -> Result<AttachRequest, super::Error> {
        let from_seq = match self.last_seq {
            None => 0,
            Some(seq) => seq
                .checked_add(1)
                .ok_or_else(|| Error::Protocol("sequence exhausted".into()))?,
        };
        Ok(AttachRequest::new(self.job_id, from_seq))
    }
}

/// Raw byte sink; task files can implement this without decoding output text.
/// Persist acceptance before returning. Output checkpoints must commit byte
/// lengths and sequence together so restart can discard an uncheckpointed tail
/// and replay it from ResumePoint. Terminal proof must be durable before success.
pub trait OutputSink {
    /// Display-only pre-acceptance status, with no durable cursor or timer effect.
    fn admission_progress(&mut self, _progress: &types::Progress) -> io::Result<()> {
        Ok(())
    }
    fn accepted(&mut self, accepted: &Accepted) -> io::Result<()>;
    fn started(&mut self, started: &Started) -> io::Result<()> {
        self.unknown_output(started.seq, &[])
    }
    fn output(&mut self, seq: u64, stream: OutputStream, bytes: &[u8]) -> io::Result<()>;
    fn truncated(&mut self, before_seq: u64) -> io::Result<()>;
    /// Commit terminal proof and the retry count before dropping a gapped stream.
    fn gap_recovery(&mut self, _recovery: &GapRecovery) -> io::Result<()> {
        Ok(())
    }
    /// Output absent after repeated recovery is lost, even when the executor
    /// did not advertise a retained-history truncation cursor.
    fn undelivered(&mut self, before_seq: u64) -> io::Result<()> {
        self.truncated(before_seq)
    }
    /// A future stream record or output descriptor is not attributed to stdout
    /// or stderr. Checkpoint its sequence and raw bytes together, without
    /// attributing an unknown descriptor's bytes to either known stream.
    fn unknown_output(&mut self, seq: u64, bytes: &[u8]) -> io::Result<()>;
    /// One piece of a file the request asked the runner to copy back. A sink
    /// that does not place returned files only advances its cursor.
    fn return_chunk(&mut self, chunk: &ReturnChunk) -> io::Result<()> {
        self.unknown_output(chunk.seq, &[])
    }
    /// The runner's per-path outcome for the requested files, sent once just
    /// before the terminal record.
    /// `expired_replay` is the seq of the last output record before the
    /// manifest, given when the manifest is a replay after the runner stopped
    /// keeping the job's files (it names an `expired` entry).
    fn return_manifest(
        &mut self,
        manifest: &ReturnManifest,
        _expired_replay: Option<u64>,
    ) -> io::Result<()> {
        self.unknown_output(manifest.seq, &[])
    }
    /// Seqs from the last committed one up to `before_seq` never arrived, and
    /// directly precede an expired replay's manifest whose runner sent no
    /// chunk counts. AFT derived the count it expected and it did not match,
    /// so these may be expired file chunks sized differently, or lost output:
    /// AFT cannot tell which.
    fn expired_gap_unverified(&mut self, before_seq: u64) -> io::Result<()> {
        self.undelivered(before_seq)
    }
    fn terminal(&mut self, record: &TerminalRecord, verdict: &Verdict) -> io::Result<()>;
}

#[derive(Default)]
pub struct StreamConsumer {
    run_cursor: Option<u64>,
    point: Option<ResumePoint>,
    accepted: bool,
    pending: BTreeMap<u64, Pending>,
    terminal: Option<Verdict>,
    recovering_gap: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Pending {
    Output(Output),
    Started(Started),
    ReturnChunk(ReturnChunk),
    ReturnManifest(ReturnManifest),
    Unknown,
}

impl StreamConsumer {
    pub fn new() -> Self {
        Self::default()
    }
    pub(crate) fn keyed_run(run_cursor: Option<u64>) -> Self {
        Self {
            run_cursor,
            ..Self::default()
        }
    }
    pub fn resume(point: ResumePoint) -> Self {
        let mut pending = point
            .gap_recovery
            .as_ref()
            .map_or_else(BTreeMap::new, |gap| gap.pending.clone());
        if let Some(last) = point.last_seq {
            pending.retain(|seq, _| *seq > last);
        }
        Self {
            recovering_gap: point.gap_recovery.is_some(),
            pending,
            point: Some(point),
            ..Self::default()
        }
    }
    pub fn resume_point(&self) -> Option<ResumePoint> {
        self.point.clone()
    }
    /// Decode a caller StreamData payload, keeping the raw sequence for future
    /// StreamRecord variants. Unknown records are never terminal; they advance
    /// the same contiguous resume cursor as output records.
    pub fn consume_bytes<S: OutputSink>(
        &mut self,
        bytes: &[u8],
        sink: &mut S,
    ) -> Result<(), Error> {
        self.consume_frame(bytes, sink).map(|_| ())
    }

    /// False means display-only data, not evidence for a recovery-budget reset.
    pub(super) fn consume_frame<S: OutputSink>(
        &mut self,
        bytes: &[u8],
        sink: &mut S,
    ) -> Result<bool, Error> {
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| Error::Protocol(e.to_string()))?;
        let seq = value.get("seq").and_then(serde_json::Value::as_u64);
        let record: StreamRecord =
            serde_json::from_value(value).map_err(|e| Error::Protocol(e.to_string()))?;
        let progress = matches!(record, StreamRecord::Progress(_));
        match record {
            StreamRecord::Progress(_)
            | StreamRecord::Accepted(_)
            | StreamRecord::Started(_)
            | StreamRecord::Output(_)
            | StreamRecord::ReturnChunk(_)
            | StreamRecord::ReturnManifest(_)
            | StreamRecord::Terminal(_) => self.consume(record, sink),
            _ => self.consume_unknown(seq, sink),
        }
        .map(|_| !progress)
    }

    /// First-seen wins for duplicate seqs, including conflicting duplicates.
    /// Out-of-order records wait until every preceding job-wide seq is present;
    /// each stream's bytes therefore retain seq order. Already committed seqs
    /// are ignored, also after restart. Bytes are never decoded chunk by chunk.
    pub fn consume<S: OutputSink>(
        &mut self,
        record: StreamRecord,
        sink: &mut S,
    ) -> Result<(), super::Error> {
        if let StreamRecord::Progress(progress) = record {
            if !self.accepted && self.point.is_none() && self.terminal.is_none() {
                sink.admission_progress(&progress)?;
            }
            return Ok(());
        }
        if self.terminal.is_some() {
            return Err(Error::Protocol("record after terminal".into()));
        }
        match record {
            StreamRecord::Accepted(accepted) => {
                if self.accepted
                    || !self.pending.is_empty()
                    || self.point.as_ref().is_some_and(|p| p.last_seq.is_some())
                {
                    return Err(Error::Protocol("acceptance is not first".into()));
                }
                self.check_job(accepted.job_id)?;
                sink.accepted(&accepted)?;
                self.point = Some(ResumePoint {
                    job_id: accepted.job_id,
                    last_seq: self.run_cursor,
                    gap_recovery: None,
                    last_output_seq: None,
                });
                self.accepted = true;
                Ok(())
            }
            StreamRecord::Started(started) => {
                self.enqueue(started.seq, Pending::Started(started), sink)
            }
            StreamRecord::Output(output) => {
                if let Some(before) = output.truncated_before_seq {
                    if before > output.seq {
                        return Err(Error::Protocol("invalid truncation cursor".into()));
                    }
                    if before > self.next_seq()? {
                        sink.truncated(before)?;
                        if let Some(point) = self.point.as_mut() {
                            point.last_seq = before.checked_sub(1);
                        }
                        self.pending.retain(|seq, _| *seq >= before);
                    }
                }
                let seq = output.seq;
                self.enqueue(seq, Pending::Output(output), sink)
            }
            StreamRecord::Terminal(record) => {
                self.check_job(record.job_id)?;
                if !self.pending.is_empty() {
                    let from_seq = self.next_seq()?;
                    let previous = self.point.as_ref().and_then(|p| p.gap_recovery.as_ref());
                    let reattaches = previous
                        .filter(|gap| gap.from_seq == from_seq)
                        .map_or(0, |gap| {
                            gap.reattaches.saturating_add(u8::from(self.recovering_gap))
                        });
                    let recovery = GapRecovery {
                        from_seq,
                        reattaches,
                        terminal: previous
                            .map_or_else(|| record.clone(), |gap| gap.terminal.clone()),
                        pending: self.pending.clone(),
                    };
                    sink.gap_recovery(&recovery)?;
                    self.point.as_mut().unwrap().gap_recovery = Some(recovery);
                    if reattaches < MAX_GAP_REATTACHES {
                        return Err(self.recovery("output sequence gap before terminal"));
                    }
                    // Only output is uncertain: the executor already supplied
                    // the outcome. Skip each hole, preserving later seq order.
                    self.lose_pending(sink)?;
                }
                if self.point.is_none() {
                    // The executor can refuse before acceptance. Lost/expired
                    // history is also a valid lone response to an attach.
                    self.point = Some(ResumePoint {
                        job_id: record.job_id,
                        last_seq: None,
                        gap_recovery: None,
                        last_output_seq: None,
                    });
                }
                let record = self
                    .point
                    .as_ref()
                    .and_then(|p| p.gap_recovery.as_ref())
                    .map_or(record, |gap| gap.terminal.clone());
                let verdict = grade(&record);
                sink.terminal(&record, &verdict)?;
                self.point.as_mut().unwrap().gap_recovery = None;
                self.terminal = Some(verdict);
                Ok(())
            }
            StreamRecord::ReturnChunk(chunk) => {
                self.check_job(chunk.job_id)?;
                let seq = chunk.seq;
                self.enqueue(seq, Pending::ReturnChunk(chunk), sink)
            }
            StreamRecord::ReturnManifest(manifest) => {
                self.check_job(manifest.job_id)?;
                let seq = manifest.seq;
                if self.skip_expired_return_chunks(&manifest)? {
                    // A retained manifest buffered for gap recovery may sit at
                    // this seq; by this reconnect the runner no longer keeps
                    // the files and replays the manifest as expired. That
                    // expired version is the one to deliver. Only this
                    // transition replaces a buffered record: a later retained
                    // replay never overwrites an expired one, and an identical
                    // duplicate stays a no-op.
                    if let Some(Pending::ReturnManifest(buffered)) = self.pending.get(&seq) {
                        if !has_expired_entry(buffered) {
                            self.pending.insert(seq, Pending::ReturnManifest(manifest));
                            return self.drain(sink);
                        }
                    }
                }
                self.enqueue(seq, Pending::ReturnManifest(manifest), sink)
            }
            StreamRecord::Unknown { seq, .. } => self.consume_unknown(seq, sink),
            _ => Err(Error::Protocol(
                "unknown typed record requires consume_bytes for its sequence".into(),
            )),
        }
    }

    fn check_job(&self, job_id: Uuid) -> Result<(), Error> {
        if self.point.as_ref().is_some_and(|p| p.job_id != job_id) {
            Err(Error::Protocol("record belongs to another job".into()))
        } else {
            Ok(())
        }
    }

    /// A replay after the runner stopped keeping a job's returned bytes sends
    /// the manifest at its original seq and no chunks, so its seq jumps over
    /// the chunks it used to send. The contract asks the caller to accept that
    /// one gap: directly before the manifest, with nothing received inside
    /// it. When a manifest still returns bytes, the missing seqs held those
    /// bytes, so the ordinary gap recovery re-attaches to fetch them.
    ///
    /// The chunks are the seqs after the job's last output record and before
    /// the manifest. Counting from that record rather than from the next
    /// expected seq handles chunks received before a reconnect, and a lost
    /// final output record shows up as one seq too many. When the manifest's
    /// entries carry `chunks`, the contract's
    /// `ReturnAssembler::finish_expired_replay` checks that count. When they
    /// do not, as from ck-motor's runner today, AFT derives it: ck-motor sends
    /// a file of size S as ceil(S / `RETURN_CHUNK_MAX_BYTES`) chunks, every
    /// one full but the last, gives the chunks consecutive seqs after output
    /// has ended and the manifest the next one, and in an expired replay keeps
    /// the manifest's seq and turns returned entries into `expired` with their
    /// original size; entries that had already failed never had chunks. The
    /// contract itself allows smaller chunks, so a derived count that does not
    /// match cannot tell lost output from differently sized chunks: after the
    /// ordinary recovery fails to fill it, that gap is reported under its own
    /// reason (`OutputSink::expired_gap_unverified`), not as lost output. A
    /// mismatch when the entries carry counts stays lost output, as the
    /// contract decides. Any gap that does not match goes to the ordinary gap
    /// recovery first, as does one with a record received inside it.
    /// Returns whether the gap was skipped.
    fn skip_expired_return_chunks(&mut self, manifest: &ReturnManifest) -> Result<bool, Error> {
        let next = self.next_seq()?;
        if manifest.total_bytes != 0
            || manifest.seq <= next
            || self.pending.range(..manifest.seq).next().is_some()
            || !has_expired_entry(manifest)
        {
            return Ok(false);
        }
        let Some(last_output_seq) = self.point.as_ref().and_then(|p| p.last_output_seq) else {
            return Ok(false);
        };
        if !expired_gap_matches(manifest, last_output_seq) {
            return Ok(false);
        }
        if let Some(point) = self.point.as_mut() {
            point.last_seq = Some(manifest.seq - 1);
        }
        Ok(true)
    }

    fn next_seq(&self) -> Result<u64, Error> {
        self.point
            .as_ref()
            .ok_or_else(|| Error::Protocol("output before acceptance or attach".into()))?
            .attach_request()
            .map(|request| request.from_seq)
    }

    fn consume_unknown<S: OutputSink>(
        &mut self,
        seq: Option<u64>,
        sink: &mut S,
    ) -> Result<(), Error> {
        if self.terminal.is_some() {
            return Err(Error::Protocol("record after terminal".into()));
        }
        if let Some(seq) = seq {
            self.enqueue(seq, Pending::Unknown, sink)?;
        }
        Ok(())
    }

    fn enqueue<S: OutputSink>(
        &mut self,
        seq: u64,
        record: Pending,
        sink: &mut S,
    ) -> Result<(), Error> {
        if seq < self.next_seq()? {
            return Ok(());
        }
        self.pending.entry(seq).or_insert(record);
        self.drain(sink)
    }

    fn drain<S: OutputSink>(&mut self, sink: &mut S) -> Result<(), Error> {
        loop {
            let next = self.next_seq()?;
            let Some(record) = self.pending.get(&next) else {
                break;
            };
            match record {
                Pending::Output(output) => match &output.stream {
                    OutputStream::Stdout | OutputStream::Stderr => {
                        sink.output(next, output.stream.clone(), &output.bytes.0)?
                    }
                    _ => sink.unknown_output(next, &output.bytes.0)?,
                },
                Pending::ReturnChunk(chunk) => sink.return_chunk(chunk)?,
                Pending::ReturnManifest(manifest) => {
                    let anchor = has_expired_entry(manifest)
                        .then(|| self.point.as_ref().and_then(|p| p.last_output_seq))
                        .flatten();
                    sink.return_manifest(manifest, anchor)?
                }
                Pending::Unknown => sink.unknown_output(next, &[])?,
                Pending::Started(started) => sink.started(started)?,
            }
            let output = matches!(record, Pending::Output(_) | Pending::Started(_));
            self.pending.remove(&next);
            if let Some(point) = self.point.as_mut() {
                point.last_seq = Some(next);
                if output {
                    point.last_output_seq = Some(next);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn finish_recovery<S: OutputSink>(
        &mut self,
        sink: &mut S,
    ) -> Result<Verdict, Error> {
        if self.terminal.is_none() {
            if let Some(gap) = self.point.as_ref().and_then(|p| p.gap_recovery.clone()) {
                // An attach need not repeat terminal proof. Its end still
                // demonstrates that this attempt did not recover the saved gap.
                self.consume(StreamRecord::Terminal(gap.terminal), sink)?;
            }
        }
        self.finish()
    }

    fn lose_pending<S: OutputSink>(&mut self, sink: &mut S) -> Result<(), Error> {
        self.drain(sink)?;
        while let Some((&before, record)) = self.pending.first_key_value() {
            match record {
                Pending::ReturnManifest(manifest) if uncounted_expired_replay(manifest) => {
                    sink.expired_gap_unverified(before)?
                }
                _ => sink.undelivered(before)?,
            }
            self.point.as_mut().unwrap().last_seq = before.checked_sub(1);
            self.drain(sink)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn finish_lost_output<S: OutputSink>(
        &mut self,
        sink: &mut S,
    ) -> Result<Verdict, Error> {
        let terminal = self
            .point
            .as_ref()
            .and_then(|p| p.gap_recovery.as_ref())
            .ok_or_else(|| self.recovery("no terminal proof for output recovery"))?
            .terminal
            .clone();
        self.lose_pending(sink)?;
        self.consume(StreamRecord::Terminal(terminal), sink)?;
        self.finish()
    }

    pub(crate) fn recovery(&self, reason: impl Into<String>) -> Error {
        Error::RecoveryRequired {
            resume: self.resume_point(),
            reason: reason.into(),
        }
    }
    pub fn finish(&self) -> Result<Verdict, super::Error> {
        self.terminal.clone().ok_or_else(|| {
            self.recovery("stream ended without a known terminal record; re-attach to the job")
        })
    }
}

/// Whether `manifest` is a replay after the runner stopped keeping the job's
/// files: an `expired` entry appears only then.
fn has_expired_entry(manifest: &ReturnManifest) -> bool {
    use super::types::{ReturnManifestOutcome, ReturnPathsFailureCause};
    manifest.files.iter().any(|entry| {
        matches!(
            &entry.outcome,
            ReturnManifestOutcome::Failed {
                cause: ReturnPathsFailureCause::Expired,
                ..
            }
        )
    })
}

/// An expired replay whose entries carry no chunk counts, so AFT has to
/// derive the count it expects.
fn uncounted_expired_replay(manifest: &ReturnManifest) -> bool {
    has_expired_entry(manifest) && manifest.files.iter().all(|entry| entry.chunks.is_none())
}

/// Whether the seqs between `last_output_seq` and the expired replay's
/// manifest are exactly the chunks the runner no longer sends.
///
/// When the entries carry `chunks`, the contract's own check decides
/// (`ReturnAssembler::finish_expired_replay`). When they carry none, as from
/// ck-motor's runner today, the count is derived: T is the sum over `expired`
/// entries of ceil(size / `RETURN_CHUNK_MAX_BYTES`), with `read_error` and
/// unrecognised entries counting zero, and the gap must equal T.
fn expired_gap_matches(manifest: &ReturnManifest, last_output_seq: u64) -> bool {
    use super::types::ReturnAssembler;
    let gap = manifest
        .seq
        .checked_sub(last_output_seq)
        .and_then(|d| d.checked_sub(1));
    if uncounted_expired_replay(manifest) {
        return gap == Some(derived_expired_chunk_count(manifest));
    }
    // A fresh assembler: the check concerns the replayed seqs, not chunks
    // received before a reconnect.
    let paths: Vec<String> = manifest
        .files
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    ReturnAssembler::new(&paths, manifest.cap_bytes)
        .finish_expired_replay(manifest, last_output_seq)
        .is_ok()
}

/// The chunks an expired replay dropped, derived from sizes for a runner that
/// sends no counts: ceil(size / `RETURN_CHUNK_MAX_BYTES`) for each `expired`
/// entry, which keeps its original size. Other failed entries, `read_error`
/// and causes this version does not know included, count zero.
fn derived_expired_chunk_count(manifest: &ReturnManifest) -> u64 {
    use super::types::{ReturnManifestOutcome, ReturnPathsFailureCause, RETURN_CHUNK_MAX_BYTES};
    manifest
        .files
        .iter()
        .filter_map(|entry| match &entry.outcome {
            ReturnManifestOutcome::Failed {
                cause: ReturnPathsFailureCause::Expired,
                size,
            } => Some(size.unwrap_or(0).div_ceil(RETURN_CHUNK_MAX_BYTES as u64)),
            _ => None,
        })
        .fold(0, u64::saturating_add)
}
