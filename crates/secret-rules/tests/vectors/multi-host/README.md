# Multi-host secret scope vectors

Copied byte for byte from the claustrum tag `secret-multi-host-vectors-v1`,
commit `cc7544d78c8c6a97b6be78f4b76cfed86651ddc7`, directory
`crates/credentials-core/tests/vectors/multi-host/`.
`SHA256SUMS` is also unchanged (its SHA-256 is
`2b0ec7ffc87a62bd027939648320f94e54fcd6a2b8af8dbf7dbd4a6cd60f0207`).

`tests/multi_host.rs` exercises all 92 scope-match cases. It also checks the
scope projections in all 11 approval, 15 flow-approval, 34 deposit/put and 8
list/permissions/operator cases that contain scope objects. The remaining two
deposit cases concern sealed-body version mismatches; four list cases concern
unreadable storage. Those cases have no scope object to parse. Vault-specific
settings, approvals, encodings and rendering are not implemented by this crate.
The vault's generator is the only writer of these JSON files; do not edit them.
