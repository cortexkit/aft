# Cingulate `tool_permission` contract vectors

`vectors/` is an unmodified copy of Cingulate's `test-vectors/tool-permission-v1/`
directory at cingulate commit `baba4382d1457b76d2590c201981c6c03f141eb3`
("mason: implement scoped AFT permission asks"). `vectors/SHA256SUMS` is
Cingulate's own manifest; AFT's tests check every listed file against it before
using the vectors.

AFT's consent client (`crates/aft/src/subc/consent.rs`) is tested against these
files in `consent_tests.rs`:

- AFT's encoded `consent.request`, `consent.await` and `consent.withdraw`
  envelopes must equal the `*-request.json` bytes (worker-open, replay,
  await-pending, await-settled, withdrawn).
- AFT's decoder must accept the `*-reply.json` bytes (worker-open, replay,
  await-pending, await-settled, withdrawn, and the ask records embedded in the
  head-not-held and escalated replies).
- `bind-stamps.json` supplies the worker and head scope stamps; only the worker
  stamp's asks carry `head_policy`.

To update, copy the directory again from the Cingulate commit that changes the
contract, update the commit named above, and rerun
`cargo test -p agent-file-tools --lib -- consent_vectors`.

## Folder-grant withdraw vectors

`scope-owner-withdraw-request.json`, `scope-owner-withdraw-reply.json`,
`non-owner-withdraw-request.json`, `non-owner-withdraw-refusal.json`,
`notice-owner-withdrawn.json`, `settled-withdraw-request.json`,
`settled-withdraw-reply.json` and `withdraw-bind-stamps.json` were additionally
copied byte for byte from Cingulate commit `04b7cbd` (tags
`access-escalated-phone-v2` and later; Cingulate HEAD `32b4ed84895c`). Their
withdraw request envelopes match `withdrawn-request.json` apart from the
elicitation id; the owner reply settles as `access_withdrawn`, the non-owner
reply refuses as `elicitation_not_owner`, and the settled reply returns the
ask's own settled record for the race guard.

## macOS informational-card pin

`macos-open-request.json`, `macos-repeat-request.json` and
`phone-macos-reply.json` were additionally copied byte for byte from Cingulate
commit `6bb5f4d`. Their hashes are unchanged from the original corpus manifest.
The closed request decoder at `crates/cingulate-decode/src/access.rs` and the
contract at `docs/contracts/consent-requester-v2.md:115-121` at that pin require
exactly `kind`, `folder`, `program` and `steps`: no `call_key`. Cingulate returns
one open id per `(folder, program)`; informational cards cannot be awaited.

The macOS vector tests check exact encoded request bytes, repeat-id semantics,
strict unknown-field rejection, and the phone projection's empty options.
