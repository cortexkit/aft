# Corpus selection and stored-value coverage

Baseline runtime: `998c896b05753dd3a83158e30f97779b89ef28e2`.
The original on-disk `tests/fixtures/callgraph` tree remains an independent
legacy/views golden workload. `corpus_manifest.json` is the exact additional
project/file inventory: every input has its original repository path and owning
test (for literals), its destination and SHA-256. `corpus_sources.json` records
the selectors and narrowly justified new inputs. No fixture program is executed.

## Selection

The corpus contains the existing 33-language scoped-node table from
`callgraph_store/position_identity_tests.rs`, the eight-language receiver table
from `callgraph_store/dispatch/tests.rs`, and the explicitly named richer projects
in the manifest. Projects are isolated from one another so similarly named types
from unrelated fixture projects cannot change resolution.

| Family | Existing richer inputs sampled |
| --- | --- |
| TypeScript / JavaScript / TSX | sample files, typed receivers, interface fanout, inheritance/super, private/unknown and builtin receivers, constructors, callback references, function values, aliased re-exports, source-less exports and JSX invocation |
| Python / Go / Rust | sample files, receiver tables, Go interfaces and Rust trait fanout, callback/value references, Rust macro templates, generated methods and unparseable macro mentions |
| Java / C# / Kotlin | receiver tables, Java field-type competition and declared constructor, Kotlin property/local construction; stored-row ordering probes additionally tie valid kind-derived node identities |
| Scala / Groovy | Scala 3 enum/given/lambda input; existing Groovy trait/interface/method/closure-property file |
| C / C++ / CUDA / Metal / Zig | free, member and pointer calls; namespace/template calls; ambiguous C++ header; CUDA launch/device calls; Metal kernel/helper call; Zig struct method |
| Ruby / Solidity / Pascal | Ruby initialize/class factory; Solidity constructor/library/modifier/interface; Pascal constructor/virtual/interface declarations |
| PHP / Swift / Lua / Perl | established namespace/trait/type/method/module inputs, Swift protocol/type alias and Perl new/bless factory |
| Structural languages | the existing JSON/YAML/TOML/HTML/SCSS smoke inputs; Vue SFC with an opaque script block; legacy Markdown headings and a negative views-plane control |

New inputs are not an invented replacement language suite. The C# overload
program pins same-named overload/arity behavior. The Rust default-trait/dynamic
receiver program exercises a single dispatch identity offered with different
provenances. Each has a `new_for` obligation in the manifest. A one-byte invalid
UTF-8 source in the lifecycle probe exists solely to exercise the legitimate
`undecodable` backend status. Its valid Rust companion reuses the established
f/g call shape; stale marking uses the production `mark_files_stale` API.

## Explicit language-by-shape limits

- Typed receiver inference is implemented for TS/TSX/JS, Python, Rust, Go, Java,
  C# and Kotlin. No typed-dispatch claim is made for the other language samples.
- Bash, SCSS, Vue, HTML, Markdown, JSON, YAML, Pascal, R, Groovy, ObjC and TOML
  have no call-expression node kinds in `calls.rs`. Their structural symbols,
  file-language values and empty/incomplete answers are still part of parity.
- Markdown is excluded by the views graph plane, not silently parsed as TS.
- No established PHP constructor/closure-call or Swift/Lua/Perl closure-call
  input was found in the selected fixture sources. Java/C#/Kotlin/C++ anonymous
  closure coverage is not claimed. Callback references and function values are
  listed as such, not mislabeled as anonymous closures.
- External imports in copied parser fixtures are deliberately unresolved. No
  React, Vue, Go package, GPU SDK or language runtime is installed or invoked.

These limits do not waive stored-value coverage. `required_values.json` lists
producer-written finite domains independently of observed output. `values.py`
compares them with the row observations and reports the case/mode/column/count
for each value, dispatch shape and unresolved-ref form in `value_inventory.json`.
Arbitrary poison strings a fault test inserts into a TEXT column are not enum
values. Missing production values fail the check rather than becoming a silent
exception. Transient `staged` refs are observed on a successful real INSERT;
ready database snapshots cannot retain them by design.

The SQLite-order probes additionally use deliberate stored-row/blob inputs to
expose bare GROUP BY representatives, unordered node selection, tied ambiguity
candidates, duplicate ordinals, target replacement order and prefix paths.
Those consumer probes supplement, rather than alter, the copied source inputs.
See `order.md` for the key and row/write-order obligations.

## Reproduce

```
python3 crates/aft/tests/store_diet/corpus.py generate
python3 crates/aft/tests/store_diet/corpus.py check
AFT_STORE_DIET_RECORD=1 cargo test -p agent-file-tools --lib -- store_diet --nocapture
python3 crates/aft/tests/store_diet/values.py record
cargo test -p agent-file-tools --lib -- store_diet
python3 crates/aft/tests/store_diet/values.py check
```

Recording refuses a schema or materialization-version bump. An encoding change
must decode rows back to these logical values and compare to the frozen oracle,
not regenerate a new expected result. The general standalone runner remains
available for the separately pinned AFT/reth trees and prepared borrowed graphs;
this fixture corpus is not a substitute for those full-tree acceptance runs.
