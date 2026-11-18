# AFT notice templates v1.1

The same opt-in `tool.catalog` replies and example `session.send` requests as
`aft-notice-templates-v1` (see its README for the fixed clock, call key and
delivery mode they use), except that every catalog now carries the
tool-provider 0.15.12 `read_only: true` mark on the tools AFT marks read-only.
The mark is always sent, so each catalog's `generation` and `catalog_digest`
change; the `*.sends.json` files are unchanged. v1 stays as published.
Regenerate with `cargo test -p agent-file-tools --lib regenerate_notice_template_vectors -- --ignored`;
`notice_template_vectors_match_samples` compares every byte.
