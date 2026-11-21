//! Bounded session clipboard registers for hashline CUT/PUT.
//!
//! Named registers persist for the life of a session binding. The anonymous
//! register is request-local. Captures that would exceed any declared bound
//! reject the whole patch in Phase 1; named registers are never silently
//! evicted to make room.

use std::collections::BTreeMap;

use crate::hashline::syntax::{
    HashlineRejection, HashlineRejectionCode, RegisterRef, RejectionStage,
};

/// Maximum number of simultaneously retained named registers in one session.
pub const MAX_NAMED_REGISTERS: usize = 64;
/// Maximum payload bytes retained in one register (joined line contents).
pub const MAX_REGISTER_BYTES: usize = 8 * 1024 * 1024;
/// Maximum payload bytes retained across every register in one session.
pub const MAX_REGISTER_TOTAL_BYTES: usize = 32 * 1024 * 1024;

/// One register's captured logical lines (content without terminators).
pub type RegisterLines = Vec<String>;

/// Session-owned register store. Mutations are applied only through
/// [`RegisterStore::commit`]; request-local staging never touches this state
/// until every planned file has an `applied*` classification.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RegisterStore {
    named: BTreeMap<String, RegisterLines>,
    anonymous: Option<RegisterLines>,
}

impl RegisterStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn named_count(&self) -> usize {
        self.named.len()
    }

    pub fn get(&self, register: &RegisterRef) -> Option<&[String]> {
        match register {
            RegisterRef::Anonymous => self.anonymous.as_deref(),
            RegisterRef::Named(name) => self.named.get(name).map(Vec::as_slice),
        }
    }

    pub fn total_bytes(&self) -> usize {
        let named: usize = self
            .named
            .values()
            .map(|lines| register_payload_bytes(lines))
            .sum();
        let anonymous = self
            .anonymous
            .as_ref()
            .map(|lines| register_payload_bytes(lines))
            .unwrap_or(0);
        named + anonymous
    }

    /// Begin a request-local fork that can stage captures without publishing
    /// them to the session until commit.
    pub fn stage(&self) -> StagedRegisters {
        StagedRegisters {
            base: self.clone(),
            working: self.clone(),
            writes: Vec::new(),
        }
    }

    /// Publish a successful request's staged captures. Callers must only invoke
    /// this when every planned file classified as `applied*`.
    pub fn commit(&mut self, staged: StagedRegisters) {
        *self = staged.working;
    }

    /// Drop a request-local fork without changing session state. Used when any
    /// planned file stops, fails, or is not attempted.
    pub fn discard(_staged: StagedRegisters) {}

    pub fn clear(&mut self) {
        self.named.clear();
        self.anonymous = None;
    }
}

/// Request-local register view used while planning and applying one patch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedRegisters {
    base: RegisterStore,
    working: RegisterStore,
    writes: Vec<RegisterWrite>,
}

/// One capture recorded during planning so commit/discard stay explicit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisterWrite {
    pub register: RegisterRef,
    pub lines: RegisterLines,
}

impl StagedRegisters {
    pub fn writes(&self) -> &[RegisterWrite] {
        &self.writes
    }

    pub fn working(&self) -> &RegisterStore {
        &self.working
    }

    pub fn base(&self) -> &RegisterStore {
        &self.base
    }

    pub fn get(&self, register: &RegisterRef) -> Option<&[String]> {
        self.working.get(register)
    }

    /// Capture lines into a register after bounds checks against the working set.
    pub fn capture(
        &mut self,
        register: RegisterRef,
        lines: RegisterLines,
    ) -> Result<(), HashlineRejection> {
        check_register_bounds(&self.working, &register, &lines)?;
        match &register {
            RegisterRef::Anonymous => {
                self.working.anonymous = Some(lines.clone());
            }
            RegisterRef::Named(name) => {
                if !self.working.named.contains_key(name)
                    && self.working.named.len() >= MAX_NAMED_REGISTERS
                {
                    return Err(register_overflow(
                        "named register count would exceed MAX_NAMED_REGISTERS",
                    ));
                }
                self.working.named.insert(name.clone(), lines.clone());
            }
        }
        self.writes.push(RegisterWrite { register, lines });
        Ok(())
    }

    /// Read a register for a PUT source. Missing named registers read as empty
    /// for gap inserts; span replacements refuse empty named/anonymous sources
    /// so a missing capture cannot silently delete the addressed range.
    pub fn read_for_put(
        &self,
        register: &RegisterRef,
        target_is_span: bool,
    ) -> Result<RegisterLines, HashlineRejection> {
        match self.get(register) {
            Some(lines) if !lines.is_empty() || !target_is_span => Ok(lines.to_vec()),
            Some(_) => Err(HashlineRejection::new(
                HashlineRejectionCode::ParseError,
                RejectionStage::Register,
                "register paste over a span requires a non-empty capture",
            )),
            None if matches!(register, RegisterRef::Named(_)) && !target_is_span => Ok(Vec::new()),
            None => Err(HashlineRejection::new(
                HashlineRejectionCode::ParseError,
                RejectionStage::Register,
                "register is empty or unknown for this PUT",
            )),
        }
    }
}

fn register_payload_bytes(lines: &[String]) -> usize {
    if lines.is_empty() {
        return 0;
    }
    lines.iter().map(|line| line.len()).sum::<usize>() + lines.len().saturating_sub(1)
}

fn check_register_bounds(
    store: &RegisterStore,
    register: &RegisterRef,
    lines: &[String],
) -> Result<(), HashlineRejection> {
    let incoming = register_payload_bytes(lines);
    if incoming > MAX_REGISTER_BYTES {
        return Err(register_overflow(
            "a single register capture exceeds MAX_REGISTER_BYTES",
        ));
    }

    let previous = store.get(register).map(register_payload_bytes).unwrap_or(0);
    let total = store.total_bytes() - previous + incoming;
    if total > MAX_REGISTER_TOTAL_BYTES {
        return Err(register_overflow(
            "register captures would exceed MAX_REGISTER_TOTAL_BYTES",
        ));
    }

    if let RegisterRef::Named(name) = register {
        if !store.named.contains_key(name) && store.named.len() >= MAX_NAMED_REGISTERS {
            return Err(register_overflow(
                "named register count would exceed MAX_NAMED_REGISTERS",
            ));
        }
    }
    Ok(())
}

fn register_overflow(message: impl Into<String>) -> HashlineRejection {
    HashlineRejection::new(
        HashlineRejectionCode::RegisterOverflow,
        RejectionStage::Register,
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_register_count_bound_rejects_without_eviction() {
        let store = RegisterStore::new();
        let mut staged = store.stage();
        for index in 0..MAX_NAMED_REGISTERS {
            staged
                .capture(RegisterRef::Named(format!("r{index}")), vec!["x".into()])
                .expect("within bound");
        }
        let err = staged
            .capture(RegisterRef::Named("overflow".into()), vec!["y".into()])
            .expect_err("one past the bound");
        assert_eq!(err.code, HashlineRejectionCode::RegisterOverflow);
        assert_eq!(err.stage, RejectionStage::Register);
        RegisterStore::discard(staged);
        assert_eq!(store.named_count(), 0);
    }

    #[test]
    fn commit_publishes_only_after_explicit_success() {
        let mut store = RegisterStore::new();
        let mut staged = store.stage();
        staged
            .capture(RegisterRef::Named("clip".into()), vec!["body".into()])
            .unwrap();
        assert!(store.get(&RegisterRef::Named("clip".into())).is_none());
        store.commit(staged);
        assert_eq!(
            store.get(&RegisterRef::Named("clip".into())),
            Some(["body".to_string()].as_slice())
        );
    }

    #[test]
    fn discard_leaves_session_registers_untouched() {
        let mut store = RegisterStore::new();
        let mut baseline = store.stage();
        baseline
            .capture(RegisterRef::Named("keep".into()), vec!["stable".into()])
            .unwrap();
        store.commit(baseline);

        let mut staged = store.stage();
        staged
            .capture(RegisterRef::Named("temp".into()), vec!["scratch".into()])
            .unwrap();
        RegisterStore::discard(staged);
        assert_eq!(
            store.get(&RegisterRef::Named("keep".into())),
            Some(["stable".to_string()].as_slice())
        );
        assert!(store.get(&RegisterRef::Named("temp".into())).is_none());
    }
}
