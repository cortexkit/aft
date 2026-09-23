use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The rendered reminder is intentionally short enough to preserve the response it annotates.
/// Gate 6 will replace this product constant with its ruled value.
pub const MAX_ALERT_LINE_CHARS: usize = 240;
pub const ALERT_ELLIPSIS: &str = "…";
pub const MAX_RENDERED_ALERT_LINES: usize = 3;

/// Commands in this closed list are transport or maintenance traffic, not agent-visible tool
/// responses. Keep additions explicit so a new command cannot silently consume a pending alert.
pub const EXCLUDED_FINALIZATION_COMMANDS: &[&str] = &[
    "configure",
    "ping",
    "version",
    "status",
    "bash_abort_inflight",
    "bash_status",
    "bash_write",
    "bash_promote",
    "bash_wait_detach",
    "bash_regex_match",
    "bash_artifact_owned",
    "bash_drain_completions",
    "bash_notify",
    "bash_unnotify",
    "bash_ack_completions",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

impl AlertSeverity {
    fn canonical_name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "information",
            Self::Hint => "hint",
        }
    }
}

/// A diagnostic accepted from one document-version-verified producer snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertDiagnostic {
    /// Canonical, dispatch-root-relative path supplied by the observation producer.
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub severity: AlertSeverity,
    pub source: Option<String>,
    pub code: Option<String>,
    pub message: String,
}

impl AlertDiagnostic {
    #[must_use]
    pub fn error(file: impl Into<String>, line: u32, message: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            line,
            column: 0,
            end_line: line,
            end_column: 0,
            severity: AlertSeverity::Error,
            source: None,
            code: None,
            message: message.into(),
        }
    }

    fn identity(&self) -> AlertIdentity {
        AlertIdentity {
            file: self.file.clone(),
            line: self.line,
            column: self.column,
            end_line: self.end_line,
            end_column: self.end_column,
            severity: self.severity.canonical_name().to_string(),
            source: self.source.clone().unwrap_or_default(),
            code: self.code.clone().unwrap_or_default(),
            message: normalize_alert_message(&self.message),
        }
    }
}

/// One complete, accepted snapshot from a single diagnostics producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertObservation {
    pub producer_key: String,
    pub diagnostics: Vec<AlertDiagnostic>,
}

impl AlertObservation {
    #[must_use]
    pub fn new(producer_key: impl Into<String>, diagnostics: Vec<AlertDiagnostic>) -> Self {
        Self {
            producer_key: producer_key.into(),
            diagnostics,
        }
    }
}

/// Canonical identity used for de-duplication, ordering ties, and rendered tracking.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AlertIdentity {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub severity: String,
    pub source: String,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AlertCandidate {
    identity: AlertIdentity,
    entered_observation_ordinal: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PartitionKey {
    dispatch_root: PathBuf,
    producer_key: String,
}

#[derive(Debug, Default)]
struct ProducerPartition {
    baseline_established: bool,
    live: BTreeMap<AlertIdentity, AlertCandidate>,
    rendered: BTreeSet<AlertIdentity>,
}

#[derive(Debug, Default)]
struct SessionAlertState {
    next_observation_ordinal: u64,
    agent_visible_response_ordinal: u64,
    partitions: BTreeMap<PartitionKey, ProducerPartition>,
}

/// Session-owned alert delta state. A host may retain one engine for its session registry.
///
/// The engine deliberately accepts a dispatch root at both observation and finalization. It
/// never consults an `AppContext` or a session project root, because a response may be scoped to
/// a root other than the context that happens to dispatch it.
#[derive(Debug, Default)]
pub struct AlertEngine {
    sessions: BTreeMap<String, SessionAlertState>,
}

/// The server-rendered result of one finalized block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedAlert {
    pub text: String,
    pub shown: Vec<AlertIdentity>,
    pub counted_only: Vec<AlertIdentity>,
    pub agent_visible_response_ordinal: u64,
}

impl RenderedAlert {
    #[must_use]
    pub fn represented_identities(&self) -> impl Iterator<Item = &AlertIdentity> {
        self.shown.iter().chain(&self.counted_only)
    }
}

impl AlertEngine {
    /// Apply one atomic authoritative-observation batch. Empty snapshots are meaningful: they
    /// prune only their producer partition and never affect another producer or root.
    pub fn observe_authoritative_batch(
        &mut self,
        session_id: &str,
        dispatch_root: &Path,
        observations: impl IntoIterator<Item = AlertObservation>,
    ) {
        let root = canonical_dispatch_root(dispatch_root);
        let state = self.sessions.entry(session_id.to_string()).or_default();
        state.next_observation_ordinal = state.next_observation_ordinal.saturating_add(1);
        let observation_ordinal = state.next_observation_ordinal;

        for observation in observations {
            let key = PartitionKey {
                dispatch_root: root.clone(),
                producer_key: observation.producer_key,
            };
            let current = observation
                .diagnostics
                .into_iter()
                .filter(|diagnostic| diagnostic.severity == AlertSeverity::Error)
                .map(|diagnostic| diagnostic.identity())
                .collect::<BTreeSet<_>>();
            let partition = state.partitions.entry(key).or_default();

            if !partition.baseline_established {
                // Gate 10 has not supplied the session-owned mutation store required to
                // attribute a first observation, so each partition establishes a silent baseline.
                partition.baseline_established = true;
                partition.live = current
                    .into_iter()
                    .map(|identity| {
                        let candidate = AlertCandidate {
                            identity: identity.clone(),
                            entered_observation_ordinal: observation_ordinal,
                        };
                        (identity, candidate)
                    })
                    .collect();
                partition.rendered = partition.live.keys().cloned().collect();
                continue;
            }

            // A disappearance closes its alert episode. Removing the rendered marker makes a
            // later re-entry a new candidate rather than a permanently suppressed identity.
            partition
                .rendered
                .retain(|identity| current.contains(identity));
            let previous_live = std::mem::take(&mut partition.live);
            partition.live = current
                .into_iter()
                .map(|identity| {
                    let candidate =
                        previous_live
                            .get(&identity)
                            .cloned()
                            .unwrap_or_else(|| AlertCandidate {
                                identity: identity.clone(),
                                entered_observation_ordinal: observation_ordinal,
                            });
                    (identity, candidate)
                })
                .collect();
        }
    }

    /// Apply one complete snapshot for one producer.
    pub fn observe_authoritative(
        &mut self,
        session_id: &str,
        dispatch_root: &Path,
        producer_key: impl Into<String>,
        diagnostics: Vec<AlertDiagnostic>,
    ) {
        self.observe_authoritative_batch(
            session_id,
            dispatch_root,
            [AlertObservation::new(producer_key, diagnostics)],
        );
    }

    /// Finalize one agent-visible response. Pending candidates are recomputed from the explicit
    /// dispatch root's `live − rendered` partitions; there is no queued cross-root work list.
    pub fn finalize(
        &mut self,
        session_id: &str,
        dispatch_root: &Path,
        command: &str,
    ) -> Option<RenderedAlert> {
        if is_excluded_finalization_command(command) {
            return None;
        }

        let root = canonical_dispatch_root(dispatch_root);
        let state = self.sessions.entry(session_id.to_string()).or_default();
        state.agent_visible_response_ordinal =
            state.agent_visible_response_ordinal.saturating_add(1);
        let response_ordinal = state.agent_visible_response_ordinal;

        let mut deliverable = Vec::new();
        for (partition_key, partition) in &state.partitions {
            if partition_key.dispatch_root != root {
                continue;
            }
            for (identity, candidate) in &partition.live {
                if !partition.rendered.contains(identity) {
                    deliverable.push((partition_key.clone(), candidate.clone()));
                }
            }
        }
        if deliverable.is_empty() {
            return None;
        }

        // Newer observations win. The identity's canonical tuple makes same-observation ordering
        // deterministic across producers and transports.
        deliverable.sort_by(|(_, left), (_, right)| {
            right
                .entered_observation_ordinal
                .cmp(&left.entered_observation_ordinal)
                .then_with(|| left.identity.cmp(&right.identity))
        });

        let shown_count = if deliverable.len() > MAX_RENDERED_ALERT_LINES - 1 {
            MAX_RENDERED_ALERT_LINES - 1
        } else {
            deliverable.len()
        };
        let shown = deliverable
            .iter()
            .take(shown_count)
            .map(|(_, candidate)| candidate.identity.clone())
            .collect::<Vec<_>>();
        let counted_only = deliverable
            .iter()
            .skip(shown_count)
            .map(|(_, candidate)| candidate.identity.clone())
            .collect::<Vec<_>>();

        let mut lines = shown.iter().map(render_alert_line).collect::<Vec<_>>();
        if !counted_only.is_empty() {
            lines.push(format!("(+{} more)", counted_only.len()));
        }
        let text = format!(
            "<system-reminder>\n{}\n</system-reminder>",
            lines.join("\n")
        );

        // Every represented identity is consumed together, including identities represented only
        // by the count suffix. This is what makes unchanged responses silent after coalescing.
        for (partition_key, candidate) in deliverable {
            if let Some(partition) = state.partitions.get_mut(&partition_key) {
                partition.rendered.insert(candidate.identity);
            }
        }

        Some(RenderedAlert {
            text,
            shown,
            counted_only,
            agent_visible_response_ordinal: response_ordinal,
        })
    }

    #[must_use]
    pub fn agent_visible_response_ordinal(&self, session_id: &str) -> u64 {
        self.sessions
            .get(session_id)
            .map_or(0, |state| state.agent_visible_response_ordinal)
    }

    #[must_use]
    pub fn partition_is_baselined(
        &self,
        session_id: &str,
        dispatch_root: &Path,
        producer_key: &str,
    ) -> bool {
        let key = PartitionKey {
            dispatch_root: canonical_dispatch_root(dispatch_root),
            producer_key: producer_key.to_string(),
        };
        self.sessions
            .get(session_id)
            .and_then(|state| state.partitions.get(&key))
            .is_some_and(|partition| partition.baseline_established)
    }

    #[must_use]
    pub fn partition_rendered_identities(
        &self,
        session_id: &str,
        dispatch_root: &Path,
        producer_key: &str,
    ) -> BTreeSet<AlertIdentity> {
        let key = PartitionKey {
            dispatch_root: canonical_dispatch_root(dispatch_root),
            producer_key: producer_key.to_string(),
        };
        self.sessions
            .get(session_id)
            .and_then(|state| state.partitions.get(&key))
            .map_or_else(BTreeSet::new, |partition| partition.rendered.clone())
    }
}

#[must_use]
pub fn is_excluded_finalization_command(command: &str) -> bool {
    EXCLUDED_FINALIZATION_COMMANDS.contains(&command)
}

/// The sole alert-message normalizer: first line, trim, whitespace-run collapse, and NFC-style
/// composition. It deliberately does not rewrite paths, numbers, or quotation marks.
#[must_use]
pub fn normalize_alert_message(message: &str) -> String {
    let first_line = message.lines().next().unwrap_or_default().trim();
    let collapsed = first_line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_ascii() {
        collapsed
    } else {
        compose_common_nfc(&collapsed)
    }
}

fn canonical_dispatch_root(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn render_alert_line(identity: &AlertIdentity) -> String {
    let line = format!(
        "New error in {}:{}: {}",
        identity.file, identity.line, identity.message
    );
    truncate_alert_line(&line)
}

fn truncate_alert_line(line: &str) -> String {
    if line.chars().count() <= MAX_ALERT_LINE_CHARS {
        return line.to_string();
    }

    let prefix_len = MAX_ALERT_LINE_CHARS.saturating_sub(ALERT_ELLIPSIS.chars().count());
    let mut truncated = line.chars().take(prefix_len).collect::<String>();
    truncated.push_str(ALERT_ELLIPSIS);
    truncated
}

/// Compose the canonical decompositions commonly emitted by diagnostics. Rust's standard library
/// exposes no full Unicode normalization facility, so unfamiliar decompositions are preserved
/// rather than applying an unsafe text rewrite outside the named normalization contract.
fn compose_common_nfc(input: &str) -> String {
    let mut normalized = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(base) = chars.next() {
        let Some(&mark) = chars.peek() else {
            normalized.push(base);
            continue;
        };
        if let Some(composed) = compose_pair(base, mark) {
            normalized.push(composed);
            chars.next();
        } else {
            normalized.push(base);
        }
    }
    normalized
}

fn compose_pair(base: char, mark: char) -> Option<char> {
    let composed = match (base, mark) {
        ('A', '\u{0300}') => 'À',
        ('A', '\u{0301}') => 'Á',
        ('A', '\u{0302}') => 'Â',
        ('A', '\u{0303}') => 'Ã',
        ('A', '\u{0308}') => 'Ä',
        ('A', '\u{030A}') => 'Å',
        ('C', '\u{0327}') => 'Ç',
        ('E', '\u{0300}') => 'È',
        ('E', '\u{0301}') => 'É',
        ('E', '\u{0302}') => 'Ê',
        ('E', '\u{0308}') => 'Ë',
        ('I', '\u{0300}') => 'Ì',
        ('I', '\u{0301}') => 'Í',
        ('I', '\u{0302}') => 'Î',
        ('I', '\u{0308}') => 'Ï',
        ('N', '\u{0303}') => 'Ñ',
        ('O', '\u{0300}') => 'Ò',
        ('O', '\u{0301}') => 'Ó',
        ('O', '\u{0302}') => 'Ô',
        ('O', '\u{0303}') => 'Õ',
        ('O', '\u{0308}') => 'Ö',
        ('U', '\u{0300}') => 'Ù',
        ('U', '\u{0301}') => 'Ú',
        ('U', '\u{0302}') => 'Û',
        ('U', '\u{0308}') => 'Ü',
        ('Y', '\u{0301}') => 'Ý',
        ('a', '\u{0300}') => 'à',
        ('a', '\u{0301}') => 'á',
        ('a', '\u{0302}') => 'â',
        ('a', '\u{0303}') => 'ã',
        ('a', '\u{0308}') => 'ä',
        ('a', '\u{030A}') => 'å',
        ('c', '\u{0327}') => 'ç',
        ('e', '\u{0300}') => 'è',
        ('e', '\u{0301}') => 'é',
        ('e', '\u{0302}') => 'ê',
        ('e', '\u{0308}') => 'ë',
        ('i', '\u{0300}') => 'ì',
        ('i', '\u{0301}') => 'í',
        ('i', '\u{0302}') => 'î',
        ('i', '\u{0308}') => 'ï',
        ('n', '\u{0303}') => 'ñ',
        ('o', '\u{0300}') => 'ò',
        ('o', '\u{0301}') => 'ó',
        ('o', '\u{0302}') => 'ô',
        ('o', '\u{0303}') => 'õ',
        ('o', '\u{0308}') => 'ö',
        ('u', '\u{0300}') => 'ù',
        ('u', '\u{0301}') => 'ú',
        ('u', '\u{0302}') => 'û',
        ('u', '\u{0308}') => 'ü',
        ('y', '\u{0301}') => 'ý',
        ('y', '\u{0308}') => 'ÿ',
        _ => return None,
    };
    Some(composed)
}

#[cfg(test)]
mod tests {
    use super::{
        compose_common_nfc, normalize_alert_message, AlertDiagnostic, AlertEngine, AlertSeverity,
        EXCLUDED_FINALIZATION_COMMANDS, MAX_ALERT_LINE_CHARS,
    };
    use std::path::Path;

    fn error(file: &str, line: u32, message: &str) -> AlertDiagnostic {
        AlertDiagnostic::error(file, line, message)
    }

    #[test]
    fn normalizer_uses_only_the_contractual_transformations() {
        assert_eq!(
            normalize_alert_message("  Cafe\u{301}   failed\nsecond line"),
            "Café failed"
        );
        assert_eq!(
            normalize_alert_message("src/a.rs:42 \"quoted\""),
            "src/a.rs:42 \"quoted\""
        );
    }

    fn normalize_alert_message_reference(message: &str) -> String {
        let first_line = message.lines().next().unwrap_or_default().trim();
        let collapsed = first_line.split_whitespace().collect::<Vec<_>>().join(" ");
        compose_common_nfc(&collapsed)
    }

    #[test]
    fn ascii_fast_path_preserves_normalized_bytes() {
        let mut messages = vec![
            String::new(),
            "  mismatched   types\tnear `request`  \nignored".to_string(),
            "Cafe\u{301}   failed".to_string(),
            "déjà vu".to_string(),
            "a\u{0327}\u{0301} unfamiliar decomposition".to_string(),
        ];
        for character in '\0'..='\u{7f}' {
            messages.push(format!(
                "  prefix{character}{character}suffix\t detail  \nignored"
            ));
        }

        for message in messages {
            assert_eq!(
                normalize_alert_message(&message).as_bytes(),
                normalize_alert_message_reference(&message).as_bytes(),
                "normalization changed for {message:?}",
            );
        }
    }

    #[test]
    fn first_observation_is_the_named_default_silent_baseline() {
        let root = Path::new("/dispatch-root");
        let mut engine = AlertEngine::default();
        engine.observe_authoritative("session", root, "server-a", vec![error("a.rs", 3, "old")]);

        assert!(engine.partition_is_baselined("session", root, "server-a"));
        assert!(engine.finalize("session", root, "read").is_none());
        assert_eq!(engine.agent_visible_response_ordinal("session"), 1);

        engine.observe_authoritative(
            "session",
            root,
            "server-a",
            vec![error("a.rs", 3, "old"), error("a.rs", 7, "new")],
        );
        let alert = engine.finalize("session", root, "read").expect("new alert");
        assert!(alert.text.contains("a.rs:7: new"));
        assert!(!alert.text.contains("your edit"));
        assert_eq!(alert.agent_visible_response_ordinal, 2);
    }

    #[test]
    fn cold_root_silence_still_advances_the_session_response_ordinal() {
        let mut engine = AlertEngine::default();
        assert!(engine
            .finalize("session", Path::new("/cold-root"), "read")
            .is_none());
        assert_eq!(engine.agent_visible_response_ordinal("session"), 1);
    }

    #[test]
    fn dispatch_root_isolation_never_borrows_or_consumes_another_root() {
        let dispatch_root = Path::new("/dispatch-root");
        let other_root = Path::new("/other-root");
        let mut engine = AlertEngine::default();
        for root in [dispatch_root, other_root] {
            engine.observe_authoritative("session", root, "server", Vec::new());
            engine.observe_authoritative(
                "session",
                root,
                "server",
                vec![error("src/lib.rs", 4, root.to_string_lossy().as_ref())],
            );
        }

        let alert = engine
            .finalize("session", dispatch_root, "inspect")
            .expect("dispatch-root alert");
        assert!(alert.text.contains("/dispatch-root"));
        assert!(!alert.text.contains("/other-root"));
        assert!(engine
            .finalize("session", other_root, "read")
            .expect("other root remains pending")
            .text
            .contains("/other-root"));
    }

    #[test]
    fn coalescing_orders_ties_and_marks_counted_identities_rendered() {
        let root = Path::new("/root");
        let mut engine = AlertEngine::default();
        engine.observe_authoritative("session", root, "server", Vec::new());
        engine.observe_authoritative(
            "session",
            root,
            "server",
            vec![
                error("z.rs", 3, "z"),
                error("a.rs", 2, "a"),
                error("m.rs", 1, "m"),
            ],
        );

        let alert = engine
            .finalize("session", root, "read")
            .expect("coalesced alert");
        assert_eq!(alert.text.lines().count(), 5);
        assert!(alert.text.find("a.rs:2").unwrap() < alert.text.find("m.rs:1").unwrap());
        assert!(alert.text.contains("(+1 more)"));
        assert_eq!(alert.represented_identities().count(), 3);
        assert_eq!(
            engine
                .partition_rendered_identities("session", root, "server")
                .len(),
            3
        );
        assert!(engine.finalize("session", root, "read").is_none());
    }

    #[test]
    fn excluded_commands_preserve_pending_alerts_and_do_not_advance_visible_ordinal() {
        let root = Path::new("/root");
        let mut engine = AlertEngine::default();
        engine.observe_authoritative("session", root, "server", Vec::new());
        engine.observe_authoritative("session", root, "server", vec![error("a.rs", 1, "boom")]);

        for command in EXCLUDED_FINALIZATION_COMMANDS {
            assert!(engine.finalize("session", root, command).is_none());
        }
        assert_eq!(engine.agent_visible_response_ordinal("session"), 0);
        assert!(engine.finalize("session", root, "inspect").is_some());
        assert_eq!(engine.agent_visible_response_ordinal("session"), 1);
    }

    #[test]
    fn warning_information_and_hints_never_enter_the_alert_channel() {
        let root = Path::new("/root");
        let mut engine = AlertEngine::default();
        for severity in [
            AlertSeverity::Warning,
            AlertSeverity::Information,
            AlertSeverity::Hint,
        ] {
            let mut diagnostic = error("a.rs", 1, "not an error");
            diagnostic.severity = severity;
            engine.observe_authoritative(
                "session",
                root,
                severity.canonical_name(),
                vec![diagnostic],
            );
        }
        assert!(engine.finalize("session", root, "read").is_none());
    }

    #[test]
    fn truncation_never_wraps_the_rendered_line() {
        let root = Path::new("/root");
        let mut engine = AlertEngine::default();
        engine.observe_authoritative("session", root, "server", Vec::new());
        engine.observe_authoritative(
            "session",
            root,
            "server",
            vec![error("a.rs", 1, &"x".repeat(MAX_ALERT_LINE_CHARS * 2))],
        );
        let alert = engine
            .finalize("session", root, "read")
            .expect("long alert");
        let line = alert.text.lines().nth(1).expect("alert line");
        assert!(line.chars().count() <= MAX_ALERT_LINE_CHARS);
        assert!(!line.contains('\n'));
    }

    #[test]
    #[ignore = "manual performance probe"]
    fn ascii_alert_normalization_perf_probe() {
        const PASSES: usize = 100_000;
        const SAMPLE_COUNT: usize = 11;
        const MESSAGES: [&str; 8] = [
            "error[E0308]:   mismatched types in crates/aft/src/commands/inspect.rs: expected `Result`, found `Option`",
            "Cannot find name 'resolvedProjectRoot'.   Did you mean 'resolveProjectRoot'?",
            "borrow of moved value: `request`   value borrowed here after move",
            "the trait bound `PathBuf: Copy` is not satisfied   required by this call",
            "Module not found: Can't resolve   '../../transport/runtime' in '/workspace/src'",
            "Property 'agent_visible_response_ordinal' does not exist on type 'AlertState'",
            "unused import: `std::collections::HashMap`   `#[warn(unused_imports)]` on by default",
            "lifetime may not live long enough   returning this value requires that `'1` must outlive `'static'",
        ];

        for message in MESSAGES {
            std::hint::black_box(normalize_alert_message(message));
        }

        let mut samples_ms = Vec::with_capacity(SAMPLE_COUNT);
        let mut checksum = 0usize;
        for _ in 0..SAMPLE_COUNT {
            let started = std::time::Instant::now();
            let mut sample_checksum = 0usize;
            for _ in 0..PASSES {
                for message in MESSAGES {
                    sample_checksum = sample_checksum.wrapping_add(
                        std::hint::black_box(normalize_alert_message(std::hint::black_box(
                            message,
                        )))
                        .len(),
                    );
                }
            }
            samples_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
            checksum = checksum.wrapping_add(std::hint::black_box(sample_checksum));
        }
        samples_ms.sort_by(f64::total_cmp);

        println!(
            "ascii-alert-normalization: calls_per_sample={} samples_ms={samples_ms:?} min_ms={:.3} median_ms={:.3} max_ms={:.3} checksum={checksum}",
            PASSES * MESSAGES.len(),
            samples_ms[0],
            samples_ms[SAMPLE_COUNT / 2],
            samples_ms[SAMPLE_COUNT - 1],
        );
    }
}
