use super::*;
use std::io;
use types::{Accepted, AttachRequest, OutputStream, StreamRecord, Uuid};

/// Durable resume cursor. `last_seq` is the last contiguous record committed
/// by the sink, not the largest sequence observed on the connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumePoint {
    pub job_id: Uuid,
    pub last_seq: Option<u64>,
}

impl ResumePoint {
    pub fn attach_request(&self) -> Result<AttachRequest, super::Error> {
        unimplemented!("inclusive replay cursor")
    }
}

/// Raw byte sink; task files can implement this without decoding output text.
/// Persist acceptance before returning, and commit output with its sequence
/// number atomically before returning, so a restart can reconstruct ResumePoint.
pub trait OutputSink {
    fn accepted(&mut self, accepted: &Accepted) -> io::Result<()>;
    fn output(&mut self, seq: u64, stream: OutputStream, bytes: &[u8]) -> io::Result<()>;
    fn truncated(&mut self, before_seq: u64) -> io::Result<()>;
    fn terminal(&mut self, record: &TerminalRecord, verdict: &Verdict) -> io::Result<()>;
}

pub struct StreamConsumer;

impl StreamConsumer {
    pub fn new() -> Self {
        Self
    }
    pub fn resume(_point: ResumePoint) -> Self {
        Self
    }
    pub fn resume_point(&self) -> Option<ResumePoint> {
        unimplemented!("persisted resume point")
    }
    pub fn consume<S: OutputSink>(
        &mut self,
        _record: StreamRecord,
        _sink: &mut S,
    ) -> Result<(), super::Error> {
        unimplemented!("raw byte reassembly")
    }
    pub fn finish(&self) -> Result<Verdict, super::Error> {
        unimplemented!("stream terminal validation")
    }
}
