AFT follows Cingulate card answers and runner send-state changes through the
existing process-lived bus connection. Events are wakes, not answers: consent
and `session.send_status` remain authoritative, and delivery is acknowledged
after local application. Replayed card answers keep the original call's
ownership and cannot consume a one-time approval twice.

The optional readers attach only to naming 0.4.2's `m_aft_cards` and
`m_aft_sendstate` rows. Missing ck-bus provisioning is non-fatal: attachment
retries with backoff, with attach state and bounded failure reasons in health.
Detached secret/vault consent calls retain their existing long-poll behavior;
attached live calls read on a wake with a 30-second backstop. Detaching interrupts
that wait. Startup reads and a ten-minute sweep backstop live generic cards.
Filesystem/path access asks do not emit bus pointers and keep their existing
`consent.await` path unchanged. Pointer deduplication uses the body's
elicitation id and feed sequence, never the message id. Pending runner
sends use a ten-minute backstop while attached and their existing retry
schedule while detached. Unknown send ids wake the bounded pending-send queue.

AFT also writes each served pin's exact `tool.views` reply to the provisioned
`aft.views` KV bucket, without creating or changing the bucket. Keys are the
lowercase hex encoding of the full UTF-8 schema pin, including tool identity
and semantics. Definitions are immutable within a process and refresh on
restart. Values over 256 KiB are skipped with a warning and health counter,
never truncated. The `tool.views` management operation is unchanged.
