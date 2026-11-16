# Card-settled bus pointers v1

One answered card filed by each of `aft`, `plexus`, `basal` and `broca`, with
the fixed example account `box_example`. Each card produces an `answers` pointer
for its daemon-stamped filing module and an `outcomes` pointer for
`prefrontal-core`. The sequence is the global `elicitation_answers` cursor shared
by both feeds, not a per-reader row count. Withdrawals produce only outcomes;
expired cards produce the same reader set as answered cards.

For each `<filer>.<reader>`, `.subject.txt` is the exact subject, `.headers.json`
encodes the publish headers, and `.body.json` contains exact body bytes. None
has a trailing newline. There is one message per reader per settlement.
`Nats-Msg-Id` is `<elicitation_id>:<reader>:<seq>` and stays unchanged on retry.
A reopened card's later settlement has a new feed position and message id.
A pointer contains no text, requester reference or answer.
Readers reread their existing feed from their own cursor; a hint is not an
answer or an acknowledgement of a feed. Publishing does not require a reader's
durable subscription (the server-side saved read cursor) to have been provisioned.

Generate from production settlement, body serialization and bus naming code:

```sh
cargo run -p cingulate-vectors --example generate_card_settled_vectors
cargo run -p cingulate-vectors --example generate_card_settled_vectors -- --check
```

The `card_settled_v1_production_bytes_and_checksums` test checks all files and
SHA256SUMS against production output. The other versioned corpora are unchanged.
