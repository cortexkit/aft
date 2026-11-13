Worker completion checks now filter saved background tasks by scope and inspect
only matching live-route sessions when aft.db is available, avoiding a full scan
of old task folders. Pending or failed task-store writes keep the check explicitly
incomplete. Health includes incomplete-reason and store-write counters, and each
completion-check log records its running count, completeness and failure reason
without logging commands.
