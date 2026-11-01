# Pinned Luau grammar build

`luau.pin.json` is the reviewed supply-chain input. Its provenance notes record
Ufuk's approval of the grammar revision, official wasi-sdk 34 archive checksum,
two-C-file build, and the Zed parse-tree exemption. Zed's archive
does not attest its compiler and flags, so it is not claimed to match the AFT
toolchain. The archive, its extracted grammar, the CLI binary, and every source
input are checksum-pinned. A mismatch fails closed; CI never updates pins.

On Linux x86-64 with Python 3.12 or newer:

```sh
python3 -m unittest discover -s grammars -p 'test_*.py' -v
python3 grammars/build.py --output target/grammar-build
```

The builder downloads only the allowlisted source files at the full commit.
It compiles `src/parser.c` and `src/scanner.c` in two fresh directories, with
the pinned SDK/flags, and requires byte equality. No grammar generation,
extension code, `build.rs`, dependency compilation or package-manager hooks
run. The external scanner runs only inside the grammar's WASM sandbox when
the prebuilt official tree-sitter CLI parses the corpus. The verifier populates
an isolated WASM-only parser cache with the exact module bytes and supplies its
own data-only grammar metadata. The pinned parser is timestamped older than
the cache; no scanner is present in that verification directory. An empty
`PATH` and nonexistent `WASI_SDK_PATH` disable compiler discovery during parsing.
The cache bytes are checked again after every load. No developer-installed
grammar or extension metadata is read.

Both builds and the Zed grammar must parse the fixed `corpus/` inputs without
errors. A, B and Zed must produce identical full XML trees, also equal to the
checked-in XML oracles (including source positions, token text and anonymous
nodes). `ifelseexpr.luau` is an unmodified real-project conformance test from
`luau-lang/luau` at `535f92589bdc304c8a2012d6cfad5e7b9faff2f7` (tag 0.701).
`features.luau` adds types, methods, require forms and scanner delimiters. The
corpus manifest records file hashes and provenance; corpus code is never run.

The job emits `artifacts/<sha256>.wasm`, `manifest-a.json`, `manifest-b.json`
and `verification.json`. B is exactly A with one extra custom section named
`aft.fixture.identity` containing `B`. Both modules are loadable and have
identical parse behavior. The checked-in fixtures and manifests live under
`crates/aft/tests/fixtures/grammars/`; they are test-only, not the production
loader manifest. The manifest URLs describe the content-addressed naming
scheme, not a claim that either test artifact has already been published.

For an intentional, reviewed pin/corpus update, use `--write-fixtures` once
and review the changed fixtures, manifests and XML oracles. CI omits that flag
and compares every generated fixture byte to Git. Keep the upstream MIT
notices with the corpus and grammar artifacts.

## Publication

Pull requests verify without write credentials. Only a push on `main` that
changes `grammars/luau.pin.json`, or a manual dispatch on `main`, can publish.
The separate publication job receives only the verified artifact bundle and
uses a narrowly scoped GitHub token; compilation never receives that token.

`publish.py` publishes A on the independent `grammars-1` release of
`cortexkit/aft`. The asset name is its complete SHA-256 plus `.wasm`. An
existing name is downloaded and byte-compared; equality succeeds without an
upload, inequality fails without replacement. Uploads never use `--clobber`.
Publication jobs serialize, and an external upload race fails rather than
replacing anything. Re-running publication is safe. B remains a test artifact.
The loader's production-manifest review must follow successful publication
and independently verify the release asset's hash and size.
