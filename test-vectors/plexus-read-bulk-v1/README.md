# Plexus bulk GitHub read vectors v1

The thirty-four JSON files (17 request/reply pairs) are copied byte for byte
from Plexus commit `51d3dc490703258bbfa5fb8dc9a50f3529463094`,
under `crates/plexus-core/tests/fixtures/gh_shim_wire/` in its
`read-bulk-requests-contract/` and `read-bulk-replies-contract/` directories.
`SHA256SUMS` pins those source bytes (run `shasum -a 256 -c SHA256SUMS` here).

Each request fixture contains the `github.read.bulk` request, scope and mock
GitHub requests and responses. Plexus generates the matching reply fixture
through its real frame handler; the complete reply frame body is identical to
the `github_read` tool's reply. Coverage includes the four read actions, size
and endpoint refusals, invalid requests, governance denial, vendor errors,
all five job-log refusal codes, dispatch failures with `unknown` and
`not_sent` transmission, and invalid read responses. Scope data describes the
upstream harness, not authority that AFT accepts from a shell request.

AFT's tests reconstruct each valid request from its parsed read contract,
reject the deliberately invalid request, and decode each reply with the shell's
real read decoder. They also check refusal retry and job-log rendering behavior.
Routing tests separately check
live advertisement changes while keeping the route's authenticated session
scope and its generation fixed.
Regenerate these files only by intentionally updating the upstream contract;
the upstream `gh_shim_wire/README.md` documents its vector generator.
