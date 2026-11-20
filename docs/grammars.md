# Downloadable grammars

Most languages AFT understands are compiled into AFT itself. A downloadable grammar adds a language without a new AFT build: AFT fetches the language's grammar the first time a project uses it. Luau is the first language delivered this way. The 33 built-in languages work as before.

## What Luau projects get

Files ending in `.luau` get outlines, symbol navigation (`aft_outline`, `aft_zoom`), call graphs (`aft_callgraph`), search, and TODO inspection. `require` paths resolve through `.luaurc` aliases.

## How a grammar is fetched and stored

- AFT carries a manifest for each downloadable grammar. It names the upstream grammar repository and commit, the license, the file size and its SHA-256 hash. Luau's grammar comes from [tree-sitter-luau](https://github.com/4teapo/tree-sitter-luau) and is published as an asset of this repository's `grammars-1` release.
- The first time a project needs the grammar, AFT downloads it once and checks its size and hash before using it. A file that doesn't match is never loaded.
- The verified grammar is stored in a `grammars` folder in AFT's cache folder: `storage.cache_dir` when you set it, otherwise AFT's storage folder. It is named by its hash, so later sessions and other projects reuse it without any network access.
- If the grammar can't be fetched (no network, or the download fails), Luau files are reported as unavailable, with the reason, rather than silently skipped. AFT tries again with growing delays, up to five attempts; after that, restarting AFT tries again.

## How a grammar is kept safe

- The grammar is WebAssembly. AFT runs it in [Wasmtime](https://wasmtime.dev)'s Pulley interpreter, never as native machine code. It has no access to files, the network or other processes.
- Every parse has a 30-second time limit, checked as parsing advances. A grammar that keeps parsing too long is stopped, and that file is reported as not parsed. The check runs between parsing steps, so it can't interrupt one step that never returns. Each grammar is pinned by hash and reviewed before it ships, which is the guard against that case.
- Because nothing is compiled to native code at runtime, AFT needs no extra permissions to run grammars. This includes the hardened runtime on macOS.

## Building AFT from source

Released AFT binaries include downloadable-grammar support on every platform.

A source build includes it only with the `wasm-grammars` feature, which needs CMake 3.18 or newer:

```bash
cargo install agent-file-tools --features wasm-grammars
```

Without the feature, AFT works as before for every built-in language and reports Luau files as unsupported by that build.

## Adding a downloadable grammar

1. Open a PR adding `grammars/<language>.pin.json` and a small, valid sample under
   `grammars/`. Leave the production manifest unchanged. Pin the upstream commit,
   source files, WASI compiler and Tree-sitter verifier. Use the pinned compiler
   and flags to determine the candidate `.wasm`'s SHA-256 and size, and record them
   as `artifact.sha256` and `artifact.size` in the pin. Set `artifact.sample` to
   the sample's repository-relative path. The **Grammar build** workflow rebuilds
   the candidate, checks its bytes against the pin, and loads it to parse the
   sample. Merging the pin PR publishes the verified hash-named asset to
   `grammars-1`; wait for that job to succeed.
2. In a second PR, add the extractor and the entry in
   `crates/aft-parse/src/grammar/manifest.json`. Copy the hash, size, and `sample`
   path from the pin and point `url` to the published asset. The loader workflow
   downloads that URL and verifies the hash and size before the manifest can ship.
