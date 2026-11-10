# AFT `permissions/v1` provider sample

`aft-provider-list.json` is AFT's reply to Cingulate's `permissions.list {}`,
in the case format of Cingulate's `test-vectors/permissions-v1/` corpus
(`method`, `now_ms`, `request`, `reply`, `route`, `tag`; `route` and `now_ms`
are test notation, not wire fields). The unit test
`permissions_list_matches_the_published_sample` (`crates/aft/src/subc/permissions_tests.rs`)
saves the grants through AFT's real saving path, pins each row's id and time
in aft.db so the reply is reproducible, lists them through the real handler
with the home folder `/Users/person`, and compares the reply with this file.

## Scenario

Head agent `agent-alf` owns worker scope `scope:wkr-7f3a`.

| id | holder | what | how it was saved |
|---|---|---|---|
| `aftg_…01` | agent `agent-alf` | read and change `~/Work/site` | person answer on card `acc_1` |
| `aftg_…02` | worker `scope:wkr-7f3a` of `agent-alf` | read `~/Work/notes` | head delegates a person-given grant, card `acc_2` |
| `aftg_…03` | worker `scope:wkr-7f3a` of `agent-alf` | read the file `~/Work/ref/spec.md` | legacy unknown source and upper-bound time |
| `aftg_…04` | worker `scope:wkr-7f3a` of `agent-alf` | read `~/Work/tools` | head delegates its default policy |

## AFT's rows

- `id`: `aftg_` and 24 hex digits, minted when the grant is first saved and kept
  while it lives; widening a read grant to read and change keeps the id and sets
  a new `granted_at_ms` and `card_ref`.
- `holder`: `{kind: "agent", agent_id}` for a head's own grant, or
  `{kind: "worker", agent_id: <owning head>, scope_ref}` for a worker's.
  `scope_ref` comes from the daemon's scope stamp. AFT never lists a `module`
  holder.
- `scope`: one of AFT's four tokens, `read` and `read_write` for a folder,
  `file_read` and `file_read_write` for one file. `scope_label` is the sentence
  the page shows; `subject_label` is the path with the home folder as `~`.
- No `expires_at_ms`, `group` or `methods`: a saved answer lasts until it is
  revoked or the holder's scope ends, and then the row is no longer listed.
- `ends_with_holder: true` on every worker row: the grant also ends when that
  worker's scope ends, so the page can say so instead of "until revoked". A
  head's grants survive its revivals, so head rows leave the member out.
- `card_ref`: the id of the Cingulate card whose answer saved the grant (for
  AFT, an access ask `acc_…`), when there was one; a grant from a worker's
  launch plan has none. Grants saved before AFT recorded it (aft.db schema 27) have
  none and show the upgrade time as `granted_at_ms`.
- `source`: `person`, `head_grant`, `head_policy` or `unknown`; all live grants
  remain visible and revocable regardless of source. Readers treat unrecognized
  source values as `unknown`.
- `granted_at_ms` is always present. Legacy rows with no stored source carry
  `granted_at_bound: "at_or_before"`: the stored time is a truthful upper bound,
  not a claim of an exact grant time. New grants with unknown lineage store
  the literal source `unknown` and have an exact time, so they omit the bound.

`provider-list-grant-sources.json` and `.jcs` are copied unchanged from
Cingulate tag `permissions-grant-sources-v1`, commit
`9d673921553cbd29d2813b4441d4cbcda47d4286`. The unit test
`permissions_list_matches_published_grant_sources_sample` checks both SHA256s
and pins the shared fields; Cingulate's app-specific fields are replaced by
separate assertions on AFT's folder fields. AFT's own sample above pins the
complete AFT reply, including provenance and time bounds.

`provider-list-ends-with-holder.json` and `.jcs` are copied unchanged from
Cingulate tag `permissions-ends-with-holder-v1`, commit `56fa35f2d8f1`. The
unit test `permissions_list_matches_published_ends_with_holder_sample` checks
both SHA256s and compares the shared fields the same way. That sample's first
row also carries `expires_at_ms`; AFT grants have no expiry, so the test drops
that field from the expected row.

`permissions.revoke {id}` answers `{outcome: "revoked"}`, adds `stopping: true`
when shell commands whose sandbox allowed that folder were still running (AFT
stops them; they end as "killed (folder access revoked)"), and answers
`{outcome: "not_found"}` for an unknown, ended or already revoked id. Commands
running on a build server (`runon`) are outside this machine's sandbox and are
not stopped. Only
`reserved:cingulate` and the local `direct` route are admitted; every other
caller gets `permissions_operator_required`.
