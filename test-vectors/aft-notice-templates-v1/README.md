# AFT notice templates v1

Opt-in `tool.catalog` replies and example `session.send` requests for each preset.
The examples use a fixed clock (1700000000000 ms), call key and cleared delivery
mode; real sends use the current clock and the runner's declared modes.
Template digests use llm-runner's RFC 8785 SHA-256 helper.

Regenerate with `cargo test -p agent-file-tools --lib regenerate_notice_template_vectors -- --ignored`.
The `notice_template_vectors_match_samples` test regenerates in memory and compares every byte.
