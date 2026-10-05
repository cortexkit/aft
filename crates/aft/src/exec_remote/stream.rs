use super::*;
use std::{collections::BTreeMap, io};
use types::{Accepted, AttachRequest, Output, OutputStream, StreamRecord, Uuid};

/// Durable resume cursor. `last_seq` is the last contiguous record committed
/// by the sink, not the largest sequence observed on the connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumePoint {
    pub job_id: Uuid,
    pub last_seq: Option<u64>,
}

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
/// Persist acceptance before returning, and commit output with its sequence
/// number atomically before returning, so a restart can reconstruct ResumePoint.
pub trait OutputSink {
    fn accepted(&mut self, accepted: &Accepted) -> io::Result<()>;
    fn output(&mut self, seq: u64, stream: OutputStream, bytes: &[u8]) -> io::Result<()>;
    fn truncated(&mut self, before_seq: u64) -> io::Result<()>;
    /// A future stream record or output descriptor is not attributed to stdout
    /// or stderr. Preserve its sequence (and raw bytes, when available) durably
    /// so reattach does not replay it forever or silently mislabel output.
    fn unknown_output(&mut self, seq: u64, bytes: &[u8]) -> io::Result<()>;
    fn terminal(&mut self, record: &TerminalRecord, verdict: &Verdict) -> io::Result<()>;
}

#[derive(Default)]
pub struct StreamConsumer {
    point: Option<ResumePoint>,
    accepted: bool,
    pending: BTreeMap<u64, Pending>,
    terminal: Option<Verdict>,
}

enum Pending {
    Output(Output),
    Unknown,
}

impl StreamConsumer {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn resume(point: ResumePoint) -> Self {
        Self {
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
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| Error::Protocol(e.to_string()))?;
        let seq = value.get("seq").and_then(serde_json::Value::as_u64);
        let record: StreamRecord =
            serde_json::from_value(value).map_err(|e| Error::Protocol(e.to_string()))?;
        match record {
            StreamRecord::Accepted(_) | StreamRecord::Output(_) | StreamRecord::Terminal(_) => {
                self.consume(record, sink)
            }
            _ => self.consume_unknown(seq, sink),
        }
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
                    last_seq: None,
                });
                self.accepted = true;
                Ok(())
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
                    return Err(self.recovery("output sequence gap before terminal"));
                }
                if self.point.is_none() {
                    // The executor can refuse before acceptance. Lost/expired
                    // history is also a valid lone response to an attach.
                    self.point = Some(ResumePoint {
                        job_id: record.job_id,
                        last_seq: None,
                    });
                }
                let verdict = grade(&record);
                sink.terminal(&record, &verdict)?;
                self.terminal = Some(verdict);
                Ok(())
            }
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
                Pending::Unknown => sink.unknown_output(next, &[])?,
            }
            self.pending.remove(&next);
            if let Some(point) = self.point.as_mut() {
                point.last_seq = Some(next);
            }
        }
        Ok(())
    }

    pub(crate) fn recovery(&self, reason: impl Into<String>) -> Error {
        Error::RecoveryRequired {
            resume: self.resume_point(),
            reason: reason.into(),
        }
    }
    pub fn finish(&self) -> Result<Verdict, super::Error> {
        self.terminal.clone().ok_or_else(|| {
            self.recovery(
                "stream ended without a known terminal; query exec.status and exec.attach",
            )
        })
    }
}
