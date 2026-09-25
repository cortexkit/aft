//! Per-command tickets that let a `gh` shim child speak through the daemon.
//!
//! An agent's children run without any subc credentials, so the `gh` shim
//! cannot prove on its own which agent session launched it. The daemon issues
//! every spawned command an unguessable ticket, hands it to the child as
//! [`GH_SHIM_TICKET_ENV`], and remembers which session and task it belongs to.
//! When the shim asks the daemon to relay a bot write, the ticket is the only
//! thing that selects the session; a session id typed on a command line or
//! placed in the environment carries no authority.
//!
//! Tickets live in this process's memory and nowhere else. They are never
//! written to task JSON, aft.db or the log, they die when their task reaches a
//! terminal state, and a daemon restart forgets all of them, so a task replayed
//! after a restart has no ticket and its shim calls are refused.

use std::sync::{LazyLock, Mutex};

/// Environment variable that carries the ticket into a spawned command.
pub const GH_SHIM_TICKET_ENV: &str = "AFT_GH_SHIM_TICKET";

const TICKET_BYTES: usize = 16;

/// The session and command a redeemed ticket was issued for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Redeemed {
    pub session_id: String,
    /// Background task id, or the id of the tool call that ran `gh` directly.
    /// Empty while a background task is still being spawned.
    pub task_id: String,
    /// Project the command ran for; used only to label the daemon's own
    /// routes to prefrontal and plexus, never to choose who speaks.
    pub project_root: String,
}

struct Entry {
    ticket: [u8; TICKET_BYTES],
    session_id: String,
    task_id: String,
    project_root: String,
}

static REGISTRY: LazyLock<Mutex<Vec<Entry>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn registry() -> std::sync::MutexGuard<'static, Vec<Entry>> {
    REGISTRY.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Whether a session may hold a ticket. Requests without an agent session use
/// the shared default namespace, which is not an agent and cannot speak.
fn session_can_hold_ticket(session_id: &str) -> bool {
    !session_id.trim().is_empty() && session_id != crate::protocol::DEFAULT_SESSION_ID
}

fn encode(ticket: &[u8; TICKET_BYTES]) -> String {
    ticket.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode(text: &str) -> Option<[u8; TICKET_BYTES]> {
    let text = text.trim();
    if text.len() != TICKET_BYTES * 2 || !text.is_ascii() {
        return None;
    }
    let mut ticket = [0_u8; TICKET_BYTES];
    for (index, slot) in ticket.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(ticket)
}

/// Compare two tickets without an early exit, so the time taken does not
/// reveal how many leading bytes of a guess were right.
fn constant_time_eq(left: &[u8; TICKET_BYTES], right: &[u8; TICKET_BYTES]) -> bool {
    let mut difference = 0_u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

fn insert(session_id: &str, task_id: &str, project_root: &str) -> Option<String> {
    if !session_can_hold_ticket(session_id) {
        return None;
    }
    let mut ticket = [0_u8; TICKET_BYTES];
    // An operating-system random source failure leaves the command without a
    // ticket, which makes its governed writes refuse rather than guessable.
    getrandom::fill(&mut ticket).ok()?;
    let encoded = encode(&ticket);
    registry().push(Entry {
        ticket,
        session_id: session_id.to_string(),
        task_id: task_id.to_string(),
        project_root: project_root.to_string(),
    });
    Some(encoded)
}

/// Look a ticket up. Every registered ticket is compared in full, so the
/// lookup time depends on the number of live tickets, not on the guess.
pub fn redeem(ticket: &str) -> Option<Redeemed> {
    let candidate = decode(ticket)?;
    let entries = registry();
    let mut found = None;
    for entry in entries.iter() {
        if constant_time_eq(&entry.ticket, &candidate) {
            found = Some(Redeemed {
                session_id: entry.session_id.clone(),
                task_id: entry.task_id.clone(),
                project_root: entry.project_root.clone(),
            });
        }
    }
    found
}

/// Forget every ticket issued for a task. Called whenever the task's metadata
/// is written in a terminal state and when its bundle is deleted.
pub fn revoke_task(task_id: &str) {
    if task_id.is_empty() {
        return;
    }
    registry().retain(|entry| entry.task_id != task_id);
}

fn revoke(ticket: &str) {
    let Some(candidate) = decode(ticket) else {
        return;
    };
    registry().retain(|entry| !constant_time_eq(&entry.ticket, &candidate));
}

/// Number of live tickets; lets tests prove a ticket was dropped.
#[cfg(all(test, unix))]
pub(crate) fn live_count_for_task(task_id: &str) -> usize {
    registry()
        .iter()
        .filter(|entry| entry.task_id == task_id)
        .count()
}

/// A ticket for a background task that has not been spawned yet, so its task
/// id is still unknown. Dropping it without [`PendingTicket::bind_task`]
/// revokes it, which covers every early return of a failed spawn.
pub struct PendingTicket {
    ticket: Option<String>,
}

impl PendingTicket {
    /// Issue a ticket for `session_id`, or none for a session that is absent
    /// or the shared default namespace.
    pub fn issue(session_id: &str, project_root: &str) -> Self {
        Self {
            ticket: insert(session_id, "", project_root),
        }
    }

    pub fn value(&self) -> Option<&str> {
        self.ticket.as_deref()
    }

    /// Attach the spawned task's id so the task's terminal transition revokes
    /// the ticket. The caller must then check whether the task already ended
    /// (see `revoke_task`), because a very short command can reach its
    /// terminal state before this runs.
    pub fn bind_task(mut self, task_id: &str) {
        let Some(ticket) = self.ticket.take() else {
            return;
        };
        let Some(candidate) = decode(&ticket) else {
            return;
        };
        for entry in registry().iter_mut() {
            if constant_time_eq(&entry.ticket, &candidate) {
                entry.task_id = task_id.to_string();
            }
        }
    }
}

impl Drop for PendingTicket {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            revoke(&ticket);
        }
    }
}

/// A ticket that lives exactly as long as one synchronous `gh` run made by
/// the daemon itself, such as a comment written through the read/edit tools.
pub struct ScopedTicket {
    ticket: Option<String>,
}

impl ScopedTicket {
    pub fn issue(session_id: &str, call_id: &str, project_root: &str) -> Self {
        Self {
            ticket: insert(session_id, call_id, project_root),
        }
    }

    pub fn value(&self) -> Option<&str> {
        self.ticket.as_deref()
    }
}

impl Drop for ScopedTicket {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            revoke(&ticket);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tickets_are_128_bit_hex_and_unique() {
        let first = ScopedTicket::issue("ses-ticket-unique", "call-a", "/p");
        let second = ScopedTicket::issue("ses-ticket-unique", "call-b", "/p");
        let first_value = first.value().unwrap();
        assert_eq!(first_value.len(), 32);
        assert!(first_value.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first_value, second.value().unwrap());
    }

    #[test]
    fn default_and_empty_sessions_get_no_ticket() {
        assert!(
            PendingTicket::issue(crate::protocol::DEFAULT_SESSION_ID, "/p")
                .value()
                .is_none()
        );
        assert!(PendingTicket::issue("", "/p").value().is_none());
        assert!(ScopedTicket::issue("  ", "call", "/p").value().is_none());
    }

    #[test]
    fn redeem_returns_the_issuing_session_and_task() {
        let pending = PendingTicket::issue("ses-redeem", "/p");
        let value = pending.value().unwrap().to_string();
        assert_eq!(
            redeem(&value),
            Some(Redeemed {
                session_id: "ses-redeem".to_string(),
                task_id: String::new(),
                project_root: "/p".to_string(),
            })
        );
        pending.bind_task("task-redeem");
        assert_eq!(redeem(&value).unwrap().task_id, "task-redeem");
        revoke_task("task-redeem");
        assert_eq!(redeem(&value), None);
    }

    #[test]
    fn fabricated_and_malformed_tickets_do_not_redeem() {
        let _live = ScopedTicket::issue("ses-fabricated", "call", "/p");
        assert_eq!(redeem("00000000000000000000000000000000"), None);
        assert_eq!(redeem("ses-fabricated"), None);
        assert_eq!(redeem(""), None);
        assert_eq!(redeem("zz000000000000000000000000000000"), None);
    }

    #[test]
    fn dropping_an_unbound_pending_ticket_revokes_it() {
        let pending = PendingTicket::issue("ses-drop", "/p");
        let value = pending.value().unwrap().to_string();
        drop(pending);
        assert_eq!(redeem(&value), None);
    }

    #[test]
    fn scoped_ticket_dies_with_its_scope() {
        let value = {
            let scoped = ScopedTicket::issue("ses-scoped", "call-scoped", "/p");
            let value = scoped.value().unwrap().to_string();
            assert!(redeem(&value).is_some());
            value
        };
        assert_eq!(redeem(&value), None);
    }
}
