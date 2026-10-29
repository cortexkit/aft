# Plexus binding-discovery contract provenance

Source repository: https://github.com/cortexkit/plexus

Source commit: `f174b2136cfddbb19f767dda217321be0272af87` (public main, owner-reported CI green).

Source vector directory: `crates/plexus-core/tests/fixtures/gh_shim_wire/bindings-contract/`.

Source README: `crates/plexus-core/tests/fixtures/gh_shim_wire/README.md`, copied as `UPSTREAM_README.md`.
Its binding-discovery section describes the real-handler producer, fixture identities, grants, and regeneration command.

The Plexus owner supplied these nine request/reply pairs and README from the named commit.
All 19 source files are copied byte-for-byte, without JSON reserialization or edits.
The request files are the github tool's arguments objects; replies are complete handler result/error envelopes, before the transport wrapper.
AFT's test stand-in reads only the vendored files. For test-specific requests it uses the test `agent_id` or `connection_id` and substitutes the test `repository`, `agent_id`, `app_handle_id`, and `repo_binding_generation` into the reply. The vector-replay test checks the vendored request/reply pairs without substitution, with the granted or ungranted caller context described in the upstream README.

The SHA-256 pins below were computed from the supplied upstream bytes and independently compared with each copied file.
The provenance test hashes raw vendored bytes, including line endings, and rejects a one-byte change that leaves the parsed JSON unchanged.
The repository's `.gitattributes` pins this directory to LF on Windows.

| File | SHA-256 |
| --- | --- |
| `agent.request.json` | `f883187792ceb225a75ff410f613d3372540c07036364243feb95b46d40ab989` |
| `agent.reply.json` | `56fc9d6e5f25e52b2c36b7cfabc61fab444f83890e6175829f3a9b1f8d474816` |
| `connection.request.json` | `762fd272f2c5c155d3ee8efac118a80947a3fcce475ce0800ad8de36c5e964d2` |
| `connection.reply.json` | `56fc9d6e5f25e52b2c36b7cfabc61fab444f83890e6175829f3a9b1f8d474816` |
| `extra-field.request.json` | `18a0afd1756c5f971017d9c98f2802e65d82b10e820f4a4bca4f288e01686127` |
| `extra-field.reply.json` | `766f0e5d056bd31fac745bd247c81f104988c63b1cd2a090a98feeff9a2639e7` |
| `missing-agent-id.request.json` | `82ad3d20c5cc888178c9319cce6ba57f2c1a6ad105569593f67838f28b18de0b` |
| `missing-agent-id.reply.json` | `32b1bfb580080811381a2b9aeb06c373a1658ecaa556fc50f5a686073c588631` |
| `no-grant.request.json` | `6bbb6172458a1b98027a879869bf47d7c8c8d41472e4339ebf3d3689aae12700` |
| `no-grant.reply.json` | `43b35599bedc1523816de94c93dbda50931953c51ee821b5549ba4cddbcb60c0` |
| `non-string-agent-id.request.json` | `0db5b96adc4e2b1f947a9a83ddd7f45fe20c01d88366a021348b82337628c9f1` |
| `non-string-agent-id.reply.json` | `32b1bfb580080811381a2b9aeb06c373a1658ecaa556fc50f5a686073c588631` |
| `none.request.json` | `23339bfdb8c55017a1cbddba1b7e04e0319190e981c8517532697eac7a263570` |
| `none.reply.json` | `43b35599bedc1523816de94c93dbda50931953c51ee821b5549ba4cddbcb60c0` |
| `other-agent.request.json` | `6bbb6172458a1b98027a879869bf47d7c8c8d41472e4339ebf3d3689aae12700` |
| `other-agent.reply.json` | `cb5c476276dd65fd963d3c1e4f2fcd5fe32daa531c13b45b9a0bf979dd3dd410` |
| `secret-extra-field.request.json` | `82dbc3f703326a102279374f959f830b17706130ab400c2dced20afd7d105a60` |
| `secret-extra-field.reply.json` | `bf90122d414731b6a4c325deb41e99bf426d7dc0767ad18875abbc667ec79fec` |
| `UPSTREAM_README.md` | `83865eb4155c7ee99eb8ae3efb4f3b39308fea8b5a622c1c55f71cf7b54593ed` |
