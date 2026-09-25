# Plexus fixtures pinned by AFT

`seen-marker-reaction.json` is a byte-for-byte copy of plexus's golden
`crates/plexus-core/tests/fixtures/github_facade/seen-marker-reaction.json`
at plexus commit `5def76f1b179f4d1dc4f5fa446319d8f48f2f3ef`. The gh shim relay
tests use its `request` (the `bot_request` arguments plexus expects) and its
`success` reply. Update it only by copying the plexus golden again.

The `CLOSED_CODES` list in `gh_shim.rs`'s relay tests is copied from plexus's
`crates/plexus-core/tests/github_acceptance/main.rs` at the same commit.
