# Plexus bulk GitHub read vectors v1

The twelve JSON files are copied byte for byte from Plexus commit `ab4b79c4`,
under `crates/plexus-core/tests/fixtures/gh_shim_wire/` in its
`read-bulk-requests-contract/` and `read-bulk-replies-contract/` directories.
`SHA256SUMS` pins those source bytes (run `shasum -a 256 -c SHA256SUMS` here).

Each request fixture contains the `github.read.bulk` request, scope and mock
GitHub requests and responses. Its matching reply fixture contains the complete legacy
reply frame body, identical to the `github_read` tool's reply. The four read
actions and the `response_too_large` and `endpoint_not_allowed` refusals are
covered. Scope data describes the upstream harness, not authority that AFT
accepts from a shell request.

AFT's tests reconstruct each request from its parsed read contract and decode
each reply with the shell's real read decoder. Routing tests separately check
live advertisement changes while keeping the route's authenticated session
scope and its generation fixed.
Regenerate these files only by intentionally updating the upstream contract;
the upstream `gh_shim_wire/README.md` documents its vector generator.
