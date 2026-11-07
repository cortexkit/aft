# AFT tokenizer

The Claude encoding tables in `src/claude_data.rs` are checked in. Ordinary
Cargo builds use them without reading a developer's npm package directory or
rewriting source files.

To refresh the tables, unpack `ai-tokenizer` under
`tmp/ai-tokenizer-pkg/package/` at the workspace root, then run:

```sh
cargo run -p aft-tokenizer --example regenerate_claude
```

Alternatively, set `AFT_TOKENIZER_CLAUDE_JS` to a Claude encoding JavaScript
file when running that command. With no available input, the command leaves
the vendored tables untouched. It keeps the generated file's historical header
and output format, so identical input produces identical bytes.
