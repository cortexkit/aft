//! One provider-wide journal; subscribers filter it without polling task files.
use super::wire::*;
use std::collections::{BTreeMap, VecDeque};

const HISTORY: usize = 4096;

#[derive(Clone)]
pub(super) enum Change {
    Upsert { before: Option<Row>, row: Row },
    Remove(Row),
    Status(Status),
}

#[derive(Clone)]
pub(super) struct Event {
    pub sequence: u64,
    pub change: Change,
}

pub(super) struct State {
    epoch: String,
    pub sequence: u64,
    pub rows: BTreeMap<String, Row>,
    pub status: Status,
    events: VecDeque<Event>,
}

impl State {
    pub(super) fn new(epoch: String) -> Self {
        Self {
            epoch,
            sequence: 0,
            rows: BTreeMap::new(),
            status: Status {
                aft: Aft {
                    version: env!("CARGO_PKG_VERSION").into(),
                    card: "building".into(),
                },
                roots: vec![],
            },
            events: VecDeque::new(),
        }
    }

    fn push(&mut self, change: Change) {
        self.sequence += 1;
        self.events.push_back(Event {
            sequence: self.sequence,
            change,
        });
        while self.events.len() > HISTORY {
            self.events.pop_front();
        }
    }

    pub(super) fn replace(&mut self, rows: Vec<Row>, status: Status) {
        let mut incoming: BTreeMap<_, _> = rows
            .into_iter()
            .map(|row| (row.task_id.clone(), row))
            .collect();
        let old_ids: Vec<_> = self.rows.keys().cloned().collect();
        for id in old_ids {
            if !incoming.contains_key(&id) {
                let row = self.rows.remove(&id).unwrap();
                self.push(Change::Remove(row));
            }
        }
        for (id, row) in std::mem::take(&mut incoming) {
            if self.rows.get(&id) != Some(&row) {
                let before = self.rows.insert(id, row.clone());
                self.push(Change::Upsert { before, row });
            }
        }
        self.update_status(status);
    }

    pub(super) fn update_status(&mut self, status: Status) {
        if self.status != status {
            self.status = status.clone();
            self.push(Change::Status(status));
        }
    }

    pub(super) fn expire(&mut self, now_ms: u64) {
        let expired: Vec<_> = self
            .rows
            .iter()
            .filter(|(_, row)| !retained(row, now_ms))
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            let row = self.rows.remove(&id).unwrap();
            self.push(Change::Remove(row));
        }
    }

    fn filter_key(filter: &Filter, status: bool) -> String {
        let mut filter = filter.clone();
        filter.cursor = None;
        blake3::hash(&serde_json::to_vec(&(filter, status)).unwrap()).to_hex()[..16].to_owned()
    }

    pub(super) fn cursor(&self, filter: &Filter, status: bool, sequence: u64) -> String {
        format!(
            "{}:{}:{sequence}",
            self.epoch,
            Self::filter_key(filter, status)
        )
    }

    pub(super) fn position(&self, filter: &Filter, status: bool, cursor: &str) -> Option<u64> {
        let prefix = format!("{}:{}:", self.epoch, Self::filter_key(filter, status));
        let position: u64 = cursor.strip_prefix(&prefix)?.parse().ok()?;
        if position > self.sequence
            || self
                .events
                .front()
                .is_some_and(|event| position + 1 < event.sequence)
        {
            return None;
        }
        Some(position)
    }

    pub(super) fn resync(&self, filter: &Filter, status: bool, reason: &str) -> Vec<WatchFrame> {
        let rows = matching_rows(self.rows.values(), filter, None);
        let count = rows.len().div_ceil(PAGE_SIZE).max(1);
        (0..count)
            .map(|part| {
                let final_part = part + 1 == count;
                let start = part * PAGE_SIZE;
                let end = (start + PAGE_SIZE).min(rows.len());
                WatchFrame::Resync {
                    reason: reason.into(),
                    part: part as u64,
                    final_part,
                    rows: rows[start..end].to_vec(),
                    status: (status && final_part)
                        .then(|| filtered_status(&self.status, filter.agent_ids.as_deref())),
                    cursor: final_part.then(|| self.cursor(filter, status, self.sequence)),
                }
            })
            .collect()
    }

    pub(super) fn replay(
        &self,
        filter: &Filter,
        status: bool,
        position: u64,
    ) -> Option<Vec<WatchFrame>> {
        if self
            .events
            .front()
            .is_some_and(|event| position + 1 < event.sequence)
        {
            return None;
        }
        Some(
            self.events
                .iter()
                .filter(|event| event.sequence > position)
                .filter_map(|event| {
                    let cursor = self.cursor(filter, status, event.sequence);
                    match &event.change {
                        Change::Upsert { row, .. } if matches(row, filter) => {
                            Some(WatchFrame::Upsert {
                                row: row.clone(),
                                cursor,
                            })
                        }
                        Change::Upsert {
                            before: Some(before),
                            row,
                        } if matches(before, filter) && !matches(row, filter) => {
                            Some(WatchFrame::Remove {
                                task_id: row.task_id.clone(),
                                cursor,
                            })
                        }
                        Change::Remove(row) if matches(row, filter) => Some(WatchFrame::Remove {
                            task_id: row.task_id.clone(),
                            cursor,
                        }),
                        Change::Status(snapshot) if status => Some(WatchFrame::Status {
                            status: filtered_status(snapshot, filter.agent_ids.as_deref()),
                            cursor,
                        }),
                        _ => None,
                    }
                })
                .collect(),
        )
    }
}

pub(super) fn retained(row: &Row, now_ms: u64) -> bool {
    row.ended_at_ms
        .is_none_or(|ended| now_ms.saturating_sub(ended) < RETENTION_MS)
}

pub(super) fn matches(row: &Row, filter: &Filter) -> bool {
    let Some(agents) = filter.agent_ids.as_ref() else {
        return true;
    };
    row.holder
        .agent_id
        .as_ref()
        .is_some_and(|id| agents.contains(id))
        && (row.holder.kind != "worker" || filter.include_workers.unwrap_or(false))
}

pub(super) fn matching_rows<'a>(
    rows: impl Iterator<Item = &'a Row>,
    filter: &Filter,
    states: Option<&[String]>,
) -> Vec<Row> {
    let mut rows: Vec<_> = rows
        .filter(|row| {
            matches(row, filter) && states.is_none_or(|states| states.contains(&row.state))
        })
        .cloned()
        .collect();
    rows.sort_by(|a, b| {
        (b.state == "running")
            .cmp(&(a.state == "running"))
            .then(b.started_at_ms.cmp(&a.started_at_ms))
            .then(a.task_id.cmp(&b.task_id))
    });
    rows
}

pub(super) fn filtered_status(status: &Status, agents: Option<&[String]>) -> Status {
    let mut status = status.clone();
    if let Some(agents) = agents {
        status.roots.retain(|root| {
            root.agent_ids_truncated == Some(true)
                || root.agent_ids.iter().any(|id| agents.contains(id))
        });
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: usize) -> Row {
        Row {
            task_id: format!("bash-{id:016x}"),
            revision: 1,
            holder: Holder {
                kind: "agent".into(),
                agent_id: Some("alf".into()),
                scope_ref: None,
                execution_id: None,
                harness: None,
            },
            session: None,
            call_id: None,
            title: None,
            command: "true".into(),
            command_truncated: None,
            cwd: "/work".into(),
            kind: "background".into(),
            remote: None,
            state: "running".into(),
            started_at_ms: id as u64,
            last_output_at_ms: None,
            ended_at_ms: None,
            output_bytes: Offsets::default(),
            exit: None,
            end_reason: None,
        }
    }

    #[test]
    fn shells_v1_filter_holder_and_retention_contract() {
        let mut agent = row(1);
        let mut worker = row(2);
        worker.holder.kind = "worker".into();
        let mut unknown = row(3);
        unknown.holder.kind = "unattributed".into();
        unknown.holder.agent_id = None;
        let filter = Filter {
            agent_ids: Some(vec!["alf".into()]),
            ..Default::default()
        };
        assert_eq!(
            matching_rows([&agent, &worker, &unknown].into_iter(), &filter, None).len(),
            1
        );
        let with_workers = Filter {
            include_workers: Some(true),
            ..filter.clone()
        };
        assert_eq!(
            matching_rows([&agent, &worker, &unknown].into_iter(), &with_workers, None).len(),
            2
        );
        assert_eq!(
            matching_rows(
                [&agent, &worker, &unknown].into_iter(),
                &Filter::default(),
                None
            )
            .len(),
            3
        );
        agent.state = "ended".into();
        agent.ended_at_ms = Some(100);
        assert!(retained(&agent, 100 + RETENTION_MS - 1));
        assert!(!retained(&agent, 100 + RETENTION_MS));
    }

    #[test]
    fn shells_v1_watch_split_resync_upsert_end_remove_and_cursor_resume() {
        let mut state = State::new("epoch".into());
        let status = state.status.clone();
        state.replace((0..51).map(row).collect(), status.clone());
        let filter = Filter::default();
        let frames = state.resync(&filter, false, "initial");
        assert_eq!(frames.len(), 2);
        assert!(
            matches!(&frames[0], WatchFrame::Resync { part:0, final_part:false, rows, cursor:None, .. } if rows.len()==50)
        );
        assert!(
            matches!(&frames[1], WatchFrame::Resync { part:1, final_part:true, rows, cursor:Some(_), .. } if rows.len()==1)
        );
        let cursor = state.cursor(&filter, false, state.sequence);
        let position = state.position(&filter, false, &cursor).unwrap();
        let mut rows: Vec<_> = state.rows.values().cloned().collect();
        rows.push(row(51));
        state.replace(rows.clone(), status.clone());
        let new = rows.last_mut().unwrap();
        new.revision = 2;
        new.state = "ended".into();
        new.ended_at_ms = Some(100);
        new.end_reason = Some("exited".into());
        state.replace(rows, status);
        state.expire(100 + RETENTION_MS);
        let events = state.replay(&filter, false, position).unwrap();
        assert!(matches!(&events[0], WatchFrame::Upsert { row, .. } if row.revision==1));
        assert!(matches!(&events[1], WatchFrame::Upsert { row, .. } if row.state=="ended"));
        assert!(
            matches!(&events[2], WatchFrame::Remove { task_id, .. } if task_id=="bash-0000000000000033")
        );
        assert!(state
            .replay(&filter, false, state.sequence)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn shells_v1_watch_lagged_resync_and_expired_or_wrong_filter_cursor_gap() {
        let mut state = State::new("epoch".into());
        let filter = Filter::default();
        let old = state.cursor(&filter, false, 0);
        for i in 0..HISTORY + 1 {
            state.push(Change::Remove(row(i)));
        }
        assert!(state.position(&filter, false, &old).is_none());
        assert!(state.replay(&filter, false, 0).is_none());
        assert!(
            matches!(&state.resync(&filter, false, "lagged")[0], WatchFrame::Resync { reason, .. } if reason=="lagged")
        );
        let current = state.cursor(&filter, false, state.sequence);
        assert!(state
            .position(
                &Filter {
                    agent_ids: Some(vec!["alf".into()]),
                    ..Default::default()
                },
                false,
                &current
            )
            .is_none());
    }
}
