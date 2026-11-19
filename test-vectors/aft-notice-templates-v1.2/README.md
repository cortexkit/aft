# AFT notice templates v1.2

These opt-in `tool.catalog` replies and example `session.send` requests follow
`aft-notice-templates-v1.1`. The head and worker catalogs have a new bash pin
for completion output tails, which also changes their `generation` and
`catalog_digest`. The reader catalog and `*.sends.json` files are unchanged.
Versions 1 and 1.1 remain as published.

Regenerate with `cargo test -p agent-file-tools --lib regenerate_notice_template_vectors -- --ignored`;
`notice_template_vectors_match_samples` compares every byte.
