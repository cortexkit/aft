# Manifest binding wire vectors

`manifest-binding-v1.json` is copied byte-for-byte from claustrum:

- Tag: `manifest-binding-vectors-v1`
- Commit: `c2a9ce2eaf99da646160eeb0c73d6b11d9341cbf`
- Path: `crates/credentials-module/tests/vectors/manifest-binding-v1.json`
- SHA-256: `8c070db5272be78e6682714a53628453e5bf7fda5a43431d4e73b4d663218433`
- Contract: `docs/wire-contract-v1.md`, section 17, SHA-256 `63aee2042c1565c47c4f3764f869b248a48a5db8a444cf0c34e2724909e731aa`

AFT consumes latest, watch, report_active, and their applicable refusals. Approval,
proposal, statement, cross-operation approval, and active_status samples belong to
operations AFT does not call and are intentionally not used by its client.

The 31 skipped sample names are: `active_status.not_ready`, `active_status.ready`,
`approve.reply`, `approve.request`, `approve.resend`, `cross.manifest_to_secret`,
`cross.secret_to_manifest`, `envelope`, `propose.reply`, `propose.request`,
`refusal.already_bound`, `refusal.approval_invalid`, `refusal.bound_to_other_agent`,
`refusal.expired`, `refusal.github_unavailable`, `refusal.identity_mismatch`,
`refusal.identity_unavailable`, `refusal.installation_all`,
`refusal.installation_too_large`, `refusal.installation_unusable`,
`refusal.manifest_setup_incomplete`, `refusal.not_installed`,
`refusal.not_reproducible`, `refusal.one_tap_disabled`, `refusal.pairing_unavailable`,
`refusal.phone_not_registered`, `refusal.secret_home_elsewhere`,
`refusal.stale_base`, `refusal.superseded`, `refusal.unknown_version`, `statement`.
