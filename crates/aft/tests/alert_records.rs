use aft::alert_records::{
    five_turn_resolution_rows, AgentVisibleFinalization, AlertRecordLogger, AlertRecordSink,
    DiagnosticIdentity, RenderedAlertBlock, RenderedAlertIdentity, RenderedIdentityDisappearance,
    Representation, WordingForm, ALERT_RENDERED_TABLE, DISAPPEARANCE_TABLE, FIVE_TURN_WINDOW,
};
use aft::harness::Harness;
use rusqlite::Connection;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use tempfile::tempdir;

#[derive(Debug, Deserialize)]
struct ReentryFixture {
    session_id: String,
    dispatch_root: String,
    first_block: BlockFixture,
    reentry_block: BlockFixture,
    first_disappearance_observation_ordinal: u64,
    second_disappearance_observation_ordinal: u64,
}

#[derive(Debug, Deserialize)]
struct BlockFixture {
    block_id: String,
    response_id: String,
    identities: Vec<IdentityFixture>,
}

#[derive(Debug, Deserialize)]
struct IdentityFixture {
    producer_key: String,
    fingerprint: String,
    file_path: String,
    line: u32,
    severity: String,
    code: Option<String>,
    wording_form: String,
    representation: String,
    lifecycle_episode_id: String,
}

#[derive(Debug, Deserialize)]
struct PendingDisappearanceFixture {
    session_id: String,
    dispatch_root: String,
    producer_key: String,
    identity_fingerprint: String,
    lifecycle_episode_id: String,
    observation_ordinal: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct PersistedRenderedRow {
    block_id: String,
    session_id: String,
    dispatch_root: String,
    producer_key: String,
    response_id: String,
    identity_fingerprint: String,
    file_path: String,
    line: u32,
    severity: String,
    code: Option<String>,
    wording_form: String,
    representation: String,
    agent_visible_response_ordinal: u64,
    lifecycle_episode_id: String,
}

fn fixture(name: &str) -> String {
    fs::read_to_string(
        crate::test_helpers::manifest_dir()
            .join("tests")
            .join("fixtures")
            .join("alert_records")
            .join(name),
    )
    .expect("read alert-record fixture")
}

fn rendered_block(fixture: &ReentryFixture, block: &BlockFixture) -> RenderedAlertBlock {
    RenderedAlertBlock {
        block_id: block.block_id.clone(),
        dispatch_root: fixture.dispatch_root.clone(),
        identities: block
            .identities
            .iter()
            .map(|identity| RenderedAlertIdentity {
                producer_key: identity.producer_key.clone(),
                diagnostic: DiagnosticIdentity {
                    fingerprint: identity.fingerprint.clone(),
                    file_path: identity.file_path.clone(),
                    line: identity.line,
                    severity: identity.severity.clone(),
                    code: identity.code.clone(),
                },
                wording_form: match identity.wording_form.as_str() {
                    "attributed" => WordingForm::Attributed,
                    "neutral" => WordingForm::Neutral,
                    unexpected => panic!("unexpected wording form {unexpected}"),
                },
                representation: match identity.representation.as_str() {
                    "shown" => Representation::Shown,
                    "counted_only" => Representation::CountedOnly,
                    unexpected => panic!("unexpected representation {unexpected}"),
                },
                lifecycle_episode_id: identity.lifecycle_episode_id.clone(),
            })
            .collect(),
    }
}

fn finalize_opencode(
    logger: &mut AlertRecordLogger,
    connection: &mut Connection,
    finalization: AgentVisibleFinalization,
) -> aft::alert_records::FinalizationLog {
    let mut sink = AlertRecordSink::for_harness(&Harness::Opencode, Some(connection))
        .expect("create OpenCode record sink");
    logger
        .finalize_agent_visible_response(finalization, &mut sink)
        .expect("finalize alert records")
}

fn note_disappearances(
    logger: &mut AlertRecordLogger,
    fixture: &ReentryFixture,
    identities: &[IdentityFixture],
    observation_ordinal: u64,
) {
    for identity in identities {
        assert!(
            logger.note_authoritative_disappearance(RenderedIdentityDisappearance {
                session_id: fixture.session_id.clone(),
                dispatch_root: fixture.dispatch_root.clone(),
                producer_key: identity.producer_key.clone(),
                identity_fingerprint: identity.fingerprint.clone(),
                lifecycle_episode_id: identity.lifecycle_episode_id.clone(),
                observation_ordinal,
            })
        );
    }
}

fn table_columns(connection: &Connection, table: &str) -> BTreeMap<String, i64> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table-info query");
    statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
        })
        .expect("run table-info query")
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()
        .expect("read table-info rows")
}

#[test]
fn opencode_rows_are_per_identity_durable_and_join_their_own_episode() {
    let fixture: ReentryFixture =
        serde_json::from_str(&fixture("five_turn_reentry.json")).expect("parse fixture");
    let directory = tempdir().expect("temporary database directory");
    let database_path = directory.path().join("opencode-alert-records.sqlite");
    let mut connection = Connection::open(&database_path).expect("open durable database");
    let mut logger = AlertRecordLogger::default();

    let first = finalize_opencode(
        &mut logger,
        &mut connection,
        AgentVisibleFinalization {
            session_id: fixture.session_id.clone(),
            response_id: fixture.first_block.response_id.clone(),
            rendered_block: Some(rendered_block(&fixture, &fixture.first_block)),
        },
    );
    assert_eq!(first.agent_visible_response_ordinal, 1);
    assert!(first.durably_written);
    assert_eq!(first.rendered_rows.len(), 2);
    assert!(first
        .rendered_rows
        .iter()
        .all(|row| row.block_id == fixture.first_block.block_id));
    assert!(first
        .rendered_rows
        .iter()
        .any(|row| row.representation == Representation::Shown));
    assert!(first
        .rendered_rows
        .iter()
        .any(|row| row.representation == Representation::CountedOnly));

    let rendered_columns = table_columns(&connection, ALERT_RENDERED_TABLE);
    for column in [
        "block_id",
        "session_id",
        "dispatch_root",
        "producer_key",
        "response_id",
        "identity_fingerprint",
        "file_path",
        "line",
        "severity",
        "code",
        "wording_form",
        "representation",
        "agent_visible_response_ordinal",
        "lifecycle_episode_id",
    ] {
        assert!(rendered_columns.contains_key(column), "missing {column}");
    }
    assert_eq!(rendered_columns["identity_fingerprint"], 1);
    assert_eq!(rendered_columns["lifecycle_episode_id"], 2);
    let persisted_render = connection
        .query_row(
            r#"
            SELECT block_id, session_id, dispatch_root, producer_key, response_id,
                   identity_fingerprint, file_path, line, severity, code, wording_form,
                   representation, agent_visible_response_ordinal, lifecycle_episode_id
            FROM alert_rendered_records
            WHERE identity_fingerprint = 'diag-a' AND lifecycle_episode_id = 'episode-a-1'
            "#,
            [],
            |row| {
                Ok(PersistedRenderedRow {
                    block_id: row.get(0)?,
                    session_id: row.get(1)?,
                    dispatch_root: row.get(2)?,
                    producer_key: row.get(3)?,
                    response_id: row.get(4)?,
                    identity_fingerprint: row.get(5)?,
                    file_path: row.get(6)?,
                    line: row.get(7)?,
                    severity: row.get(8)?,
                    code: row.get(9)?,
                    wording_form: row.get(10)?,
                    representation: row.get(11)?,
                    agent_visible_response_ordinal: row.get(12)?,
                    lifecycle_episode_id: row.get(13)?,
                })
            },
        )
        .expect("read complete durable alert-rendered row");
    assert_eq!(
        persisted_render,
        PersistedRenderedRow {
            block_id: "block-first".to_string(),
            session_id: fixture.session_id.clone(),
            dispatch_root: fixture.dispatch_root.clone(),
            producer_key: "rust-analyzer".to_string(),
            response_id: "response-1".to_string(),
            identity_fingerprint: "diag-a".to_string(),
            file_path: "src/lib.rs".to_string(),
            line: 17,
            severity: "error".to_string(),
            code: Some("E0308".to_string()),
            wording_form: "attributed".to_string(),
            representation: "shown".to_string(),
            agent_visible_response_ordinal: 1,
            lifecycle_episode_id: "episode-a-1".to_string(),
        }
    );

    note_disappearances(
        &mut logger,
        &fixture,
        &fixture.first_block.identities,
        fixture.first_disappearance_observation_ordinal,
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM alert_disappearance_records",
                [],
                |row| row.get::<_, u64>(0),
            )
            .expect("count pre-finalization rows"),
        0,
        "authoritative detection must not fabricate a response ordinal"
    );

    let first_disappearance_finalization = finalize_opencode(
        &mut logger,
        &mut connection,
        AgentVisibleFinalization {
            session_id: fixture.session_id.clone(),
            response_id: "response-2".to_string(),
            rendered_block: None,
        },
    );
    assert_eq!(
        first_disappearance_finalization.agent_visible_response_ordinal,
        2
    );
    assert_eq!(first_disappearance_finalization.disappearance_rows.len(), 2);
    assert!(first_disappearance_finalization
        .disappearance_rows
        .iter()
        .all(|row| row.agent_visible_response_ordinal == 2));

    let silent = finalize_opencode(
        &mut logger,
        &mut connection,
        AgentVisibleFinalization {
            session_id: fixture.session_id.clone(),
            response_id: "response-3".to_string(),
            rendered_block: None,
        },
    );
    assert_eq!(silent.agent_visible_response_ordinal, 3);
    assert!(silent.rendered_rows.is_empty());
    assert!(silent.disappearance_rows.is_empty());

    let reentry = finalize_opencode(
        &mut logger,
        &mut connection,
        AgentVisibleFinalization {
            session_id: fixture.session_id.clone(),
            response_id: fixture.reentry_block.response_id.clone(),
            rendered_block: Some(rendered_block(&fixture, &fixture.reentry_block)),
        },
    );
    assert_eq!(reentry.agent_visible_response_ordinal, 4);
    assert_eq!(reentry.rendered_rows.len(), 2);

    note_disappearances(
        &mut logger,
        &fixture,
        &fixture.reentry_block.identities,
        fixture.second_disappearance_observation_ordinal,
    );
    let second_disappearance_finalization = finalize_opencode(
        &mut logger,
        &mut connection,
        AgentVisibleFinalization {
            session_id: fixture.session_id.clone(),
            response_id: "response-5".to_string(),
            rendered_block: None,
        },
    );
    assert_eq!(
        second_disappearance_finalization.agent_visible_response_ordinal,
        5
    );

    let disappearance_columns = table_columns(&connection, DISAPPEARANCE_TABLE);
    for column in [
        "session_id",
        "dispatch_root",
        "producer_key",
        "identity_fingerprint",
        "lifecycle_episode_id",
        "observation_ordinal",
        "agent_visible_response_ordinal",
    ] {
        assert!(
            disappearance_columns.contains_key(column),
            "missing {column}"
        );
    }
    assert_eq!(disappearance_columns["identity_fingerprint"], 1);
    assert_eq!(disappearance_columns["lifecycle_episode_id"], 2);
    let persisted_disappearance = connection
        .query_row(
            r#"
            SELECT session_id, dispatch_root, producer_key, identity_fingerprint,
                   lifecycle_episode_id, observation_ordinal, agent_visible_response_ordinal
            FROM alert_disappearance_records
            WHERE identity_fingerprint = 'diag-a' AND lifecycle_episode_id = 'episode-a-2'
            "#,
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, u64>(5)?,
                    row.get::<_, u64>(6)?,
                ))
            },
        )
        .expect("read complete durable disappearance row");
    assert_eq!(
        persisted_disappearance,
        (
            fixture.session_id.clone(),
            fixture.dispatch_root.clone(),
            "rust-analyzer".to_string(),
            "diag-a".to_string(),
            "episode-a-2".to_string(),
            fixture.second_disappearance_observation_ordinal,
            5,
        )
    );

    let rows = five_turn_resolution_rows(&connection).expect("run committed five-turn query");
    assert_eq!(rows.len(), 4, "every shown and counted-only row must join");
    assert!(rows.iter().any(|row| row.representation == "shown"));
    assert!(rows.iter().any(|row| row.representation == "counted_only"));
    assert!(rows.iter().all(|row| {
        row.disappearance_ordinal > row.rendered_ordinal
            && row.disappearance_ordinal - row.rendered_ordinal <= FIVE_TURN_WINDOW
    }));
    let joined_episodes = rows
        .iter()
        .map(|row| {
            (
                row.identity_fingerprint.as_str(),
                row.lifecycle_episode_id.as_str(),
                row.rendered_ordinal,
                row.disappearance_ordinal,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        joined_episodes,
        vec![
            ("diag-a", "episode-a-1", 1, 2),
            ("diag-b", "episode-b-1", 1, 2),
            ("diag-a", "episode-a-2", 4, 5),
            ("diag-b", "episode-b-2", 4, 5),
        ],
        "a re-entry must never join its former lifecycle episode"
    );

    drop(connection);
    let reopened = Connection::open(&database_path).expect("reopen durable database");
    assert_eq!(
        reopened
            .query_row("SELECT COUNT(*) FROM alert_rendered_records", [], |row| {
                row.get::<_, u64>(0)
            })
            .expect("count durable rendered rows"),
        4
    );
    assert_eq!(
        reopened
            .query_row(
                "SELECT COUNT(*) FROM alert_disappearance_records",
                [],
                |row| { row.get::<_, u64>(0) }
            )
            .expect("count durable disappearance rows"),
        4
    );
}

#[test]
fn pending_disappearance_is_not_completed_when_session_ends() {
    let fixture: PendingDisappearanceFixture =
        serde_json::from_str(&fixture("session_end_with_pending_disappearance.json"))
            .expect("parse pending-disappearance fixture");
    let mut connection = Connection::open_in_memory().expect("open database");
    let mut logger = AlertRecordLogger::default();
    let block = RenderedAlertBlock {
        block_id: "block-ending".to_string(),
        dispatch_root: fixture.dispatch_root.clone(),
        identities: vec![RenderedAlertIdentity {
            producer_key: fixture.producer_key.clone(),
            diagnostic: DiagnosticIdentity {
                fingerprint: fixture.identity_fingerprint.clone(),
                file_path: "src/ending.rs".to_string(),
                line: 1,
                severity: "error".to_string(),
                code: None,
            },
            wording_form: WordingForm::Neutral,
            representation: Representation::Shown,
            lifecycle_episode_id: fixture.lifecycle_episode_id.clone(),
        }],
    };

    finalize_opencode(
        &mut logger,
        &mut connection,
        AgentVisibleFinalization {
            session_id: fixture.session_id.clone(),
            response_id: "response-rendered".to_string(),
            rendered_block: Some(block),
        },
    );
    assert!(
        logger.note_authoritative_disappearance(RenderedIdentityDisappearance {
            session_id: fixture.session_id.clone(),
            dispatch_root: fixture.dispatch_root,
            producer_key: fixture.producer_key,
            identity_fingerprint: fixture.identity_fingerprint,
            lifecycle_episode_id: fixture.lifecycle_episode_id,
            observation_ordinal: fixture.observation_ordinal,
        })
    );

    logger.close_session(&fixture.session_id);
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM alert_disappearance_records",
                [],
                |row| row.get::<_, u64>(0),
            )
            .expect("count disappearance rows"),
        0,
        "a closed session has no next finalized response from which to obtain an ordinal"
    );
}

#[test]
fn non_opencode_hosts_construct_rows_without_durable_writes() {
    let mut logger = AlertRecordLogger::default();
    let mut sink = AlertRecordSink::for_harness(&Harness::Runner, None)
        .expect("non-OpenCode sink is intentionally disabled");
    let log = logger
        .finalize_agent_visible_response(
            AgentVisibleFinalization {
                session_id: "runner-session".to_string(),
                response_id: "runner-response".to_string(),
                rendered_block: Some(RenderedAlertBlock {
                    block_id: "runner-block".to_string(),
                    dispatch_root: "/work/runner".to_string(),
                    identities: vec![RenderedAlertIdentity {
                        producer_key: "rust-analyzer".to_string(),
                        diagnostic: DiagnosticIdentity {
                            fingerprint: "runner-diagnostic".to_string(),
                            file_path: "src/lib.rs".to_string(),
                            line: 8,
                            severity: "error".to_string(),
                            code: Some("E0308".to_string()),
                        },
                        wording_form: WordingForm::Neutral,
                        representation: Representation::Shown,
                        lifecycle_episode_id: "runner-episode-1".to_string(),
                    }],
                }),
            },
            &mut sink,
        )
        .expect("construct rows for a non-OpenCode response");

    assert_eq!(log.agent_visible_response_ordinal, 1);
    assert_eq!(log.rendered_rows.len(), 1);
    assert!(!log.durably_written);
    assert!(!sink.is_durable());
}
