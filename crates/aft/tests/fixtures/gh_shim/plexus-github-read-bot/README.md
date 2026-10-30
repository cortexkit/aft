# Plexus github_read vectors (copied)

Byte-for-byte copies from the plexus repository, commit `019f6bbd`, path
`crates/plexus-module/tests/fixtures/github-read-bot/`. Do not edit or
re-format these files; copy a new revision from Plexus instead.

Rule they pin: in a `github.actions.job_log` reply, `offset`, `length` and
`total_bytes` count raw log bytes, while `body` is display text (ANSI codes
stripped, split characters decoded as U+FFFD, secrets redacted), so the body's
length need not equal `length`.
