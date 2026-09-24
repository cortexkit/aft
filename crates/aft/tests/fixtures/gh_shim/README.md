# gh routing shim fixtures

Fixture corpus for the `gh` routing shim (`crates/aft/src/gh_shim.rs`).

The manifest/envelope fixtures in this directory are dev-signed test
material. Release builds compile an EMPTY trust set, so none of these
signatures can move a release binary past R2; production trust material
arrives with the separately reviewed custody ceremony release.

## Envelope v2 shape

One file on disk carries the whole artifact:

```json
{
  "artifact_id": "gh-routing-manifest",
  "envelope_version": 2,
  "key_id": "<signing key id>",
  "fetched_at_unix_secs": 0,
  "signature": "<base64 Ed25519 signature>",
  "manifest_bytes": "<the exact published manifest file, as a JSON string>"
}
```

Signing contract: the signature covers `manifest_bytes` AS DISTRIBUTED — the
exact bytes of the manifest file the signer publishes. The verifier verifies
the received bytes FIRST and parses them SECOND. There is no canonicalization
rule: re-formatting, re-ordering, or re-encoding a signed manifest changes the
bytes and breaks the signature. `fetched_at_unix_secs` is advisory local
metadata only. `issued_at_unix_secs` lives inside the signed manifest bytes
and is displayed as provenance in `--status`; it does not expire the artifact.

`initial-manifest-v1.json` is the authoritative signed byte source for the
canonical fixtures: its file contents, byte for byte, are what the canonical
signature covers. Do not re-format it; the envelope fixtures and the
`signed_envelope_fixtures_match_their_generator` test fail if it drifts from
the signatures. Regenerate the envelopes with:

```sh
AFT_GH_SHIM_REGEN=1 cargo test -p agent-file-tools gh_shim --lib
```

## CKCRED prover pack (dev material)

External provers can reproduce every dev signature with this material.

Dev live test key (compiled into debug builds only):

| field | value |
| --- | --- |
| key_id | `gh-routing-dev-test-key-v1` |
| algorithm | Ed25519 |
| seed hex | `9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60` |
| public_key hex | `d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a` |

Dev standby fixture key (test-only; NOT in the compiled dev trust set — used
to exercise the two-slot trust-set mechanics against an injected set):

| field | value |
| --- | --- |
| key_id | `gh-routing-dev-standby-key-v1` |
| algorithm | Ed25519 |
| seed hex (ASCII of the string shown) | `gh-shim-standby-fixture-seed-001` |
| public_key hex | `33c96b995909f8fb8f1be1fc597bb63645c3b776d299ae464ff4b6a4857b07bd` |

Canonical signed bytes: the exact contents of `initial-manifest-v1.json`
(sha256 `4029bbc056a45fef56b41256d2adacaa1cab64a34c5c063c9d943d78d75de8b5`),
signed with the dev live test key. Expected signature base64:

```
IwrC9VYvTyUjPSAGrw13+2beSZ3E0ECTcJtJDOBQgZGUP51NWvC4CfmfUDXwdcFddTqvN6D40KLdm/HgQE52Bg==
```

## Oracle cases

| fixture | case | expected |
| --- | --- | --- |
| `signed-envelope-v2.json` | raw-bytes round-trip golden: bytes -> verify -> parse | signature verifies over the exact `initial-manifest-v1.json` bytes; parse yields the fixture manifest |
| `signed-envelope-v2-tampered.json` | tampered single byte | same signature as canonical, one substituted byte inside `manifest_bytes` (`issue view` -> `issue View`): verification fails |
| `signed-envelope-v2-future-issued-at.json` | future `issued_at_unix_secs` (issue time + 3600s, beyond the 300s skew) | refused as invalid |
| `signed-envelope-v2-stale-issued-at.json` | aged `issued_at_unix_secs` (issue time - 2,000,000s) | accepted and continues to classify governed commands; its timestamp remains visible as provenance in `--status` |
| `signed-envelope-v2-version-2.json` | newer `manifest_version` (2), same body | accepted; sets the version high-water mark. After it is accepted, `signed-envelope-v2.json` (version 1, validly signed) is refused as a rollback incident, visible in `--status` as `gh_shim_status_manifest_rollback` |
| `signed-envelope-v2-standby-key.json` | standby-key signature over the canonical bytes | accepted under a two-slot trust set containing the standby key; a third unknown key id is refused |

The regressed-invalid-artifact oracle is composed from the fixtures above:
accept the canonical envelope, then replace the artifact with the tampered
envelope. The validation failure immediately makes governed and admin tuples
classified by the last-valid manifest refuse with `gh_shim_manifest_regressed`;
mechanical operations pass through. Time passage does not participate. See
`regressed_invalid_artifact_refuses_governed_and_admin_and_passes_mechanical`
in `crates/aft/src/gh_shim.rs`.

## Untouched wire fixtures

The wire-schema golden `gh-route-result-v1-golden.json`, `holder-responses-v1.json`,
`classification-v1.json`, `mechanical-r3-v1.json`,
`governed-regressions-v1.json`, and `self-report-v1.json` describe the
holder-facing wire and classification contract; the envelope v2 change does
not touch them. The request bytes the shim actually sends are captured, with a
drift test, in `docs/investigations/gh-shim-wire-goldens/requests/`.
