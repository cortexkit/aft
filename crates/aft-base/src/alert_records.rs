//! Durable observation records for the alert channel.
//!
//! The alert engine owns diagnostics acceptance and lifecycle-id minting. This
//! module receives those server-side events, constructs one row per represented
//! identity, and persists rows only when the active sink is OpenCode.

use crate::harness::Harness;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::fmt;

pub const ALERT_RENDERED_TABLE: &str = "alert_rendered_records";
pub const DISAPPEARANCE_TABLE: &str = "alert_disappearance_records";
pub const FIVE_TURN_WINDOW: u64 = 5;

/// A predefined offline query for the five-turn observation window. It joins
/// lifecycle episodes rather than trying to infer them by parsing reminder prose.
pub const FIVE_TURN_RESOLUTION_QUERY: &str = r#"
SELECT
    rendered.identity_fingerprint,
    rendered.representation,
    rendered.producer_key,
    rendered.lifecycle_episode_id,
    rendered.agent_visible_response_ordinal AS rendered_ordinal,
    disappearance.agent_visible_response_ordinal AS disappearance_ordinal
FROM alert_rendered_records AS rendered
JOIN alert_disappearance_records AS disappearance
  ON rendered.producer_key = disappearance.producer_key
 AND rendered.lifecycle_episode_id = disappearance.lifecycle_episode_id
WHERE disappearance.agent_visible_response_ordinal
      BETWEEN rendered.agent_visible_response_ordinal + 1
          AND rendered.agent_visible_response_ordinal + 5
ORDER BY rendered.agent_visible_response_ordinal, rendered.identity_fingerprint
"#;

const ALERT_RECORD_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS alert_rendered_records (
    block_id                         TEXT NOT NULL,
    session_id                       TEXT NOT NULL,
    dispatch_root                    TEXT NOT NULL,
    producer_key                     TEXT NOT NULL,
    response_id                      TEXT NOT NULL,
    identity_fingerprint             TEXT NOT NULL,
    file_path                        TEXT NOT NULL,
    line                             INTEGER NOT NULL CHECK (line > 0),
    severity                         TEXT NOT NULL,
    code                             TEXT,
    wording_form                     TEXT NOT NULL CHECK (wording_form IN ('attributed', 'neutral')),
    representation                   TEXT NOT NULL CHECK (representation IN ('shown', 'counted_only')),
    agent_visible_response_ordinal   INTEGER NOT NULL CHECK (agent_visible_response_ordinal > 0),
    lifecycle_episode_id             TEXT NOT NULL,
    PRIMARY KEY (identity_fingerprint, lifecycle_episode_id)
);
CREATE INDEX IF NOT EXISTS idx_alert_rendered_producer_episode
    ON alert_rendered_records (producer_key, lifecycle_episode_id);

CREATE TABLE IF NOT EXISTS alert_disappearance_records (
    session_id                       TEXT NOT NULL,
    dispatch_root                    TEXT NOT NULL,
    producer_key                     TEXT NOT NULL,
    identity_fingerprint             TEXT NOT NULL,
    lifecycle_episode_id             TEXT NOT NULL,
    observation_ordinal              INTEGER NOT NULL CHECK (observation_ordinal > 0),
    agent_visible_response_ordinal   INTEGER NOT NULL CHECK (agent_visible_response_ordinal > 0),
    PRIMARY KEY (identity_fingerprint, lifecycle_episode_id)
);
CREATE INDEX IF NOT EXISTS idx_alert_disappearance_producer_episode
    ON alert_disappearance_records (producer_key, lifecycle_episode_id);
"#;

#[derive(Debug)]
pub enum AlertRecordError {
    Sqlite(rusqlite::Error),
    MissingOpenCodeSink,
    DuplicateRenderedIdentity {
        identity_fingerprint: String,
        lifecycle_episode_id: String,
    },
}

impl fmt::Display for AlertRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(f, "alert record database error: {error}"),
            Self::MissingOpenCodeSink => {
                write!(f, "an OpenCode session requires an alert record database sink")
            }
            Self::DuplicateRenderedIdentity {
                identity_fingerprint,
                lifecycle_episode_id,
            } => write!(
                f,
                "finalized block represents {identity_fingerprint} more than once in lifecycle episode {lifecycle_episode_id}"
            ),
        }
    }
}

impl std::error::Error for AlertRecordError {}

impl From<rusqlite::Error> for AlertRecordError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// Prepare the database tables used only by the OpenCode-hosted measurement sink.
pub fn ensure_schema(connection: &Connection) -> Result<(), AlertRecordError> {
    connection.execute_batch(ALERT_RECORD_SCHEMA)?;
    Ok(())
}

/// A sink chosen by the host. Disabled sinks still receive constructed rows in
/// [`FinalizationLog`], but intentionally perform no durable write.
pub enum AlertRecordSink<'connection> {
    OpenCode(&'connection mut Connection),
    Disabled,
}

impl<'connection> AlertRecordSink<'connection> {
    pub fn for_harness(
        harness: &Harness,
        connection: Option<&'connection mut Connection>,
    ) -> Result<Self, AlertRecordError> {
        if !matches!(harness, Harness::Opencode) {
            return Ok(Self::Disabled);
        }

        let connection = connection.ok_or(AlertRecordError::MissingOpenCodeSink)?;
        ensure_schema(connection)?;
        Ok(Self::OpenCode(connection))
    }

    pub fn is_durable(&self) -> bool {
        matches!(self, Self::OpenCode(_))
    }

    fn persist(
        &mut self,
        rendered_rows: &[AlertRenderedRecord],
        disappearance_rows: &[DisappearanceRecord],
    ) -> Result<(), AlertRecordError> {
        let Self::OpenCode(connection) = self else {
            return Ok(());
        };

        let transaction = connection.transaction()?;
        for row in rendered_rows {
            transaction.execute(
                r#"
                INSERT OR IGNORE INTO alert_rendered_records (
                    block_id, session_id, dispatch_root, producer_key, response_id,
                    identity_fingerprint, file_path, line, severity, code, wording_form,
                    representation, agent_visible_response_ordinal, lifecycle_episode_id
                )
                VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14
                )
                "#,
                params![
                    row.block_id,
                    row.session_id,
                    row.dispatch_root,
                    row.producer_key,
                    row.response_id,
                    row.identity_fingerprint,
                    row.file_path,
                    row.line,
                    row.severity,
                    row.code,
                    row.wording_form.as_str(),
                    row.representation.as_str(),
                    row.agent_visible_response_ordinal,
                    row.lifecycle_episode_id,
                ],
            )?;
        }

        for row in disappearance_rows {
            transaction.execute(
                r#"
                INSERT OR IGNORE INTO alert_disappearance_records (
                    session_id, dispatch_root, producer_key, identity_fingerprint,
                    lifecycle_episode_id, observation_ordinal,
                    agent_visible_response_ordinal
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                "#,
                params![
                    row.session_id,
                    row.dispatch_root,
                    row.producer_key,
                    row.identity_fingerprint,
                    row.lifecycle_episode_id,
                    row.observation_ordinal,
                    row.agent_visible_response_ordinal,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticIdentity {
    pub fingerprint: String,
    pub file_path: String,
    pub line: u32,
    pub severity: String,
    pub code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordingForm {
    Attributed,
    Neutral,
}

impl WordingForm {
    fn as_str(self) -> &'static str {
        match self {
            Self::Attributed => "attributed",
            Self::Neutral => "neutral",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Representation {
    Shown,
    CountedOnly,
}

impl Representation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Shown => "shown",
            Self::CountedOnly => "counted_only",
        }
    }
}

/// An identity represented by a finalized alert block. Every entry, including a
/// `CountedOnly` entry represented solely by `(+N more)`, becomes its own row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedAlertIdentity {
    pub producer_key: String,
    pub diagnostic: DiagnosticIdentity,
    pub wording_form: WordingForm,
    pub representation: Representation,
    pub lifecycle_episode_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedAlertBlock {
    pub block_id: String,
    pub dispatch_root: String,
    pub identities: Vec<RenderedAlertIdentity>,
}

/// A finalized agent-visible response. The logger receives one of these for
/// every agent-visible response, including responses with no alert block, so the
/// session-global response ordinal remains contiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentVisibleFinalization {
    pub session_id: String,
    pub response_id: String,
    pub rendered_block: Option<RenderedAlertBlock>,
}

/// The accepted-observation event supplied by the alert delta engine when an
/// identity that it had already rendered leaves its producer partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedIdentityDisappearance {
    pub session_id: String,
    pub dispatch_root: String,
    pub producer_key: String,
    pub identity_fingerprint: String,
    pub lifecycle_episode_id: String,
    pub observation_ordinal: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertRenderedRecord {
    pub block_id: String,
    pub session_id: String,
    pub dispatch_root: String,
    pub producer_key: String,
    pub response_id: String,
    pub identity_fingerprint: String,
    pub file_path: String,
    pub line: u32,
    pub severity: String,
    pub code: Option<String>,
    pub wording_form: WordingForm,
    pub representation: Representation,
    pub agent_visible_response_ordinal: u64,
    pub lifecycle_episode_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisappearanceRecord {
    pub session_id: String,
    pub dispatch_root: String,
    pub producer_key: String,
    pub identity_fingerprint: String,
    pub lifecycle_episode_id: String,
    pub observation_ordinal: u64,
    pub agent_visible_response_ordinal: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizationLog {
    pub agent_visible_response_ordinal: u64,
    pub rendered_rows: Vec<AlertRenderedRecord>,
    pub disappearance_rows: Vec<DisappearanceRecord>,
    pub durably_written: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RenderedEpisodeKey {
    session_id: String,
    producer_key: String,
    identity_fingerprint: String,
    lifecycle_episode_id: String,
}

/// Session-local bookkeeping that turns accepted observation and finalization
/// events into record rows. It intentionally has no response rendering logic.
#[derive(Default)]
pub struct AlertRecordLogger {
    response_ordinals: HashMap<String, u64>,
    rendered_episodes: HashSet<RenderedEpisodeKey>,
    pending_disappearances: Vec<RenderedIdentityDisappearance>,
}

impl AlertRecordLogger {
    /// Queue a disappearance only when this exact lifecycle episode was rendered.
    /// The durable row is deliberately deferred until the next agent-visible
    /// finalization supplies a response ordinal.
    pub fn note_authoritative_disappearance(
        &mut self,
        disappearance: RenderedIdentityDisappearance,
    ) -> bool {
        let key = RenderedEpisodeKey {
            session_id: disappearance.session_id.clone(),
            producer_key: disappearance.producer_key.clone(),
            identity_fingerprint: disappearance.identity_fingerprint.clone(),
            lifecycle_episode_id: disappearance.lifecycle_episode_id.clone(),
        };
        if !self.rendered_episodes.remove(&key) {
            return false;
        }

        self.pending_disappearances.push(disappearance);
        true
    }

    /// Finalize an agent-visible response, incrementing the session-global
    /// ordinal even when there is no alert block. Pending disappearances for the
    /// session receive this ordinal; a session closed before this call receives none.
    pub fn finalize_agent_visible_response(
        &mut self,
        finalization: AgentVisibleFinalization,
        sink: &mut AlertRecordSink<'_>,
    ) -> Result<FinalizationLog, AlertRecordError> {
        let agent_visible_response_ordinal = self
            .response_ordinals
            .get(&finalization.session_id)
            .copied()
            .unwrap_or_default()
            + 1;

        let disappearance_rows = self
            .pending_disappearances
            .iter()
            .filter(|pending| pending.session_id == finalization.session_id)
            .map(|pending| DisappearanceRecord {
                session_id: pending.session_id.clone(),
                dispatch_root: pending.dispatch_root.clone(),
                producer_key: pending.producer_key.clone(),
                identity_fingerprint: pending.identity_fingerprint.clone(),
                lifecycle_episode_id: pending.lifecycle_episode_id.clone(),
                observation_ordinal: pending.observation_ordinal,
                agent_visible_response_ordinal,
            })
            .collect::<Vec<_>>();

        let rendered_rows = finalization
            .rendered_block
            .as_ref()
            .map(|block| {
                block
                    .identities
                    .iter()
                    .map(|identity| AlertRenderedRecord {
                        block_id: block.block_id.clone(),
                        session_id: finalization.session_id.clone(),
                        dispatch_root: block.dispatch_root.clone(),
                        producer_key: identity.producer_key.clone(),
                        response_id: finalization.response_id.clone(),
                        identity_fingerprint: identity.diagnostic.fingerprint.clone(),
                        file_path: identity.diagnostic.file_path.clone(),
                        line: identity.diagnostic.line,
                        severity: identity.diagnostic.severity.clone(),
                        code: identity.diagnostic.code.clone(),
                        wording_form: identity.wording_form,
                        representation: identity.representation,
                        agent_visible_response_ordinal,
                        lifecycle_episode_id: identity.lifecycle_episode_id.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        validate_rendered_rows(&rendered_rows)?;
        sink.persist(&rendered_rows, &disappearance_rows)?;

        self.response_ordinals.insert(
            finalization.session_id.clone(),
            agent_visible_response_ordinal,
        );
        self.pending_disappearances
            .retain(|pending| pending.session_id != finalization.session_id);
        self.rendered_episodes
            .extend(rendered_rows.iter().map(|row| RenderedEpisodeKey {
                session_id: row.session_id.clone(),
                producer_key: row.producer_key.clone(),
                identity_fingerprint: row.identity_fingerprint.clone(),
                lifecycle_episode_id: row.lifecycle_episode_id.clone(),
            }));

        Ok(FinalizationLog {
            agent_visible_response_ordinal,
            rendered_rows,
            disappearance_rows,
            durably_written: sink.is_durable(),
        })
    }

    /// Discard in-memory session state at session close. Pending disappearances
    /// are not fabricated into rows because no agent-visible response supplied an ordinal.
    pub fn close_session(&mut self, session_id: &str) {
        self.response_ordinals.remove(session_id);
        self.pending_disappearances
            .retain(|pending| pending.session_id != session_id);
        self.rendered_episodes
            .retain(|key| key.session_id != session_id);
    }
}

fn validate_rendered_rows(rows: &[AlertRenderedRecord]) -> Result<(), AlertRecordError> {
    let mut keys = HashSet::with_capacity(rows.len());
    for row in rows {
        let key = (
            row.identity_fingerprint.as_str(),
            row.lifecycle_episode_id.as_str(),
        );
        if !keys.insert(key) {
            return Err(AlertRecordError::DuplicateRenderedIdentity {
                identity_fingerprint: row.identity_fingerprint.clone(),
                lifecycle_episode_id: row.lifecycle_episode_id.clone(),
            });
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiveTurnResolutionRow {
    pub identity_fingerprint: String,
    pub representation: String,
    pub producer_key: String,
    pub lifecycle_episode_id: String,
    pub rendered_ordinal: u64,
    pub disappearance_ordinal: u64,
}

/// Execute the predefined five-turn measurement query and return its rows. This
/// function intentionally does not calculate an efficacy rate.
pub fn five_turn_resolution_rows(
    connection: &Connection,
) -> Result<Vec<FiveTurnResolutionRow>, AlertRecordError> {
    let mut statement = connection.prepare(FIVE_TURN_RESOLUTION_QUERY)?;
    let rows = statement.query_map([], |row| {
        Ok(FiveTurnResolutionRow {
            identity_fingerprint: row.get(0)?,
            representation: row.get(1)?,
            producer_key: row.get(2)?,
            lifecycle_episode_id: row.get(3)?,
            rendered_ordinal: row.get(4)?,
            disappearance_ordinal: row.get(5)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}
