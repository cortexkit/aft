use super::{types::*, Error};

/// Unary caller replies, not executor-to-runner frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Prepare(PrepareReply),
    Drop(DropReply),
    Cancel(CancelReply),
    Status(StatusReply),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplyVerdict {
    Prepared,
    /// A refused or unknown prepare outcome never establishes a warm workspace.
    WorkspaceUnprepared {
        reason: Option<RefusalReason>,
    },
    Dropped {
        existed: bool,
    },
    CancelAcknowledged,
    /// Unknown rebuild results count as unavailable, not successful builds.
    Status {
        reachable: bool,
        has_unknown_rebuild: bool,
    },
}

pub fn decode_reply(operation: &str, bytes: &[u8]) -> Result<Reply, Error> {
    let error = |e: serde_json::Error| Error::Protocol(e.to_string());
    match operation {
        "workspace.prepare" => serde_json::from_slice(bytes)
            .map(Reply::Prepare)
            .map_err(error),
        "workspace.drop" => serde_json::from_slice(bytes)
            .map(Reply::Drop)
            .map_err(error),
        "exec.cancel" => serde_json::from_slice(bytes)
            .map(Reply::Cancel)
            .map_err(error),
        "exec.status" => serde_json::from_slice(bytes)
            .map(Reply::Status)
            .map_err(error),
        _ => Err(Error::Protocol(format!(
            "not a unary caller operation: {operation}"
        ))),
    }
}

/// Preserve full replies separately from their grade, including nullable status
/// metadata. Unknown prepare/rebuild tags never claim prepared/successful state.
pub fn grade_reply(reply: &Reply) -> ReplyVerdict {
    match reply {
        Reply::Prepare(reply) => match &reply.outcome {
            PrepareOutcome::Prepared => ReplyVerdict::Prepared,
            PrepareOutcome::RefusedBeforeStart { reason } => ReplyVerdict::WorkspaceUnprepared {
                reason: Some(reason.clone()),
            },
            _ => ReplyVerdict::WorkspaceUnprepared { reason: None },
        },
        Reply::Drop(reply) => ReplyVerdict::Dropped {
            existed: reply.dropped,
        },
        Reply::Cancel(_) => ReplyVerdict::CancelAcknowledged,
        Reply::Status(reply) => ReplyVerdict::Status {
            reachable: reply.server_reachable,
            has_unknown_rebuild: reply.repositories.iter().any(|repo| {
                !matches!(
                    repo.last_rebuild_result,
                    None | Some(
                        RebuildResult::Building | RebuildResult::Ok | RebuildResult::Failed
                    )
                )
            }),
        },
    }
}
