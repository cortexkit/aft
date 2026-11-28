# Served permission contract corpus, v1

This corpus pins the [requester v2](../../docs/contracts/consent-requester-v2.md)
and [phone v2](../../docs/contracts/consent-phone-v2.md) contracts. Existing v1
corpora are unchanged. Each `*-request.json` is a framed request; its `*-reply.json`
is untouched real handler response bytes. `bind-stamps.json` supplies the trusted
fixture identity. The clock is fixed at 1000 ms and every generation uses a fresh
scratch store. The macOS fixture explicitly models macOS even on Linux.

Scenarios, in order: worker opening and head-leg get; refused phone head-leg
answer; unheld head grant refusal; escalation with a note; phone get and pending
list; user scope grant applying to the worker; losing late head answer; persistent
call-key replay; read-only settled await; head's own user ask and immediate pending
await; withdrawal; informational macOS opening, repeat and phone get. The five
`notice-*.json` files are actual durable outbox bodies, not hand-written proxies.

Regenerate locally (this command writes source-controlled vectors):

```
cargo run -p cingulate-vectors --example generate_access_vectors --locked
```

Check without writes:

```
cargo run -p cingulate-vectors --example generate_access_vectors --locked -- --check
cargo test -p cingulate-vectors --test access --locked
```

`SHA256SUMS` covers every file except itself. Tests independently verify directory
coverage and every hash, pin the manifest's SHA256, and regenerate served bytes.
When changing this contract, review the schema and bytes together, regenerate,
and deliberately update the manifest pin. A pin is an integrity boundary, not a
substitute for the real-handler permission rule tests.
