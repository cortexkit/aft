# OpenCode 2 Docker harness

`../../run-opencode2-test.sh` is the local and CI entry point. It reads the V2 beta pin from the existing load-matrix source, builds the checkout binary and plugin tarball in Docker, verifies producer-backed executable provenance, validates all registrations, and runs the selected matrix.

Tool slices add `registration.json`, `registration.ts`, or `*.scenario.json` below `../scenarios/<tool>/`. The loader discovers these files recursively; harness changes are not needed. Optional `*.extension.ts` files may implement `HarnessExtension` for slice-owned validation and lifecycle assertions. The JSON shapes are documented by `registration.schema.json` and `matrix.schema.json`.

Every scenario gets private `HOME`, `TMPDIR`, and `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, and `XDG_RUNTIME_DIR` roots. Forensics remain under `AFT_E2E_ARTIFACT_ROOT`. Use `AFT_E2E_SCENARIO=<tool>/<trajectory>` to run one parent row.

The contract-owning slice places transcript-backed host contracts under `../contract/`. Host-origin T2 scenarios fail validation with `contract_uncaptured:host_schema_rejection` until that capture exists. To hand the contract slice a fresh pinned-host observation, set `AFT_OPENCODE2_SCHEMA_OBSERVATION=1` and select a host-origin T2 scenario; the runner writes `host-schema-rejection.observation.json` to the artifact directory without claiming ownership of the committed contract file.
