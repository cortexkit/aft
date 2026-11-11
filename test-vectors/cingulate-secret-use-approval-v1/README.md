# Secret-use approvals v1

`secret-approval-v1.json` is copied byte for byte from Claustrum's
`secret-approval-vectors-v1` tag, commit `cefc5c7041ffe93d17c4fcc80e5847260f3ad121`,
at `crates/credentials-core/tests/vectors/secret-approval-v1.json`.
Its SHA-256 is `efd4ba60641586e143bce81087e4f9f5aaf8a99ffede74190303da7d4495cc27`.
These are public deterministic test keys, never production credentials.

`single-phone-get.json` and `batch-phone-get.json` are untouched response bytes
from Cingulate's real `consent.get` path on a stamped phone route advertising
`clientFeatures: ["secret-use-approval"]`. They file the `approval` and
`batch-omitted` statements respectively, at 1700000000000 ms, in fresh scratch
stores. Phone records use `secretUse`; its nested vault items retain snake_case.
The phone renders requested access from the statements, not the generic title.

Filer samples are framed `consent.request` and `consent.await` calls filed as
`reserved:aft` with the vault's `approval` statement (`batch-omitted` for the
partial card), at 1700000000000 ms in fresh scratch stores. Each file holds the
exact method, request params and handler reply envelope, so AFT and Cerebellum
can build decoders against real replies. Filer records use `snake_case` with
`secret_use` and `secret_use_answer`; refusals carry `code` and `message`.

| File | What it shows |
|---|---|
| `secret-approval-v1.json` | Vault goldens copied byte for byte, never production credentials. |
| `single-phone-get.json` | Phone `consent.get` for the `approval` statement. |
| `batch-phone-get.json` | Phone `consent.get` for the `batch-omitted` statements. |
| `filer-request.json` | Filer `consent.request` reply with the new `elicitation_id`. |
| `filer-request-repeat.json` | Same filing repeated, returning the same `elicitation_id`. |
| `filer-await-pending.json` | Filer `consent.await` with zero timeout before any answer, pending. |
| `filer-await-allowed.json` | Filer `consent.await` after an allow envelope, with `secret_use_answer`. |
| `filer-await-partial.json` | Filer `consent.await` for the batch card after its one-item envelope. |
| `filer-await-declined.json` | Filer `consent.await` after a decline, every item declined. |
| `filer-await-expired.json` | Filer `consent.await` after the earliest expiry passes on the test clock. |
| `filer-refusals.json` | Four `consent.request` refusals: agent mismatch, non-canonical, id reuse, broca. |

Regenerate with `cargo run -p cingulate-vectors --example generate_secret_use_vectors --locked`.
Use `--check` to compare without writing. The `secret_use` integration tests
regenerate and compare both served records byte for byte, exercise the three
use-approval cases through filing/answering, and verify checksum coverage.
