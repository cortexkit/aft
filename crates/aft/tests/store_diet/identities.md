# Identity and text-reader inventory

Runtime baseline: the named main commit in `baseline.json`. The source-line
references below were first inventoried at `998c896b05753dd3a83158e30f97779b89ef28e2`;
constructor/reader names and `source_inventory.json.gz:id_text_readers` identify later
locations for the text-sensitive readers.
`table_encoding.json.gz` lists columns individually. These are **planned encodings**;
this harness does not change a database format or discard a column.

## Constructor inputs that must survive encoding

`H(parts)` below is BLAKE3 of the NUL-joined text parts. Decimal numbers use the
same spelling as the baseline, including the original ordinal's zero/one base.
The full salt-zero digest must be recomputed from the inputs, not from the stored
64-bit prefix, when deriving another identity. Salted identities hash the
little-endian eight-byte salt followed by the complete salt-zero digest.

| Key | Identity inputs / deterministic mapping | Constructor at baseline |
| --- | --- | --- |
| `nodes.id`, symbol | length-delimited relative path, node-kind text, scoped name; `pos:symbol:` form tag | `callgraph_store/mod.rs:15940-15955` |
| `nodes.id`, top level | relative path alone; `top:` form tag | same |
| views `nodes.id` | base node inputs above; former text `view:symbol:<base-id>` | same; `views/materialization.rs:707` |
| callable `refs.ref_id` | caller file, original hash-kind argument, caller scoped symbol, line, byte start, byte end, full callee text, per-file callable ordinal | `callgraph_store/mod.rs:16008-16060` |
| macro-template / macro-mention `refs.ref_id` | the same callable constructor, with original hash-kind `macro_body_call` or `macro_mention`; macro-template `local_name` is subsequently replaced by the macro caller's `name!`, not the callee name | `mod.rs:15798`, `15976-15994`; `callgraph.rs` macro extraction |
| import `refs.ref_id` | caller file, `import`, byte start, byte end, module path, import index | `mod.rs:16062-16105` |
| Rust module `refs.ref_id` | caller file, `module`, module name, node start byte | `mod.rs:16107-16192` |
| JS re-export `refs.ref_id` | caller file, `reexport`, statement start/end bytes, module path, re-export ordinal | `mod.rs:16601-16736` |
| Rust re-export `refs.ref_id` | caller file, `rust_reexport`, import start/end bytes, module path, re-export ordinal | same |
| export-alias `refs.ref_id` | caller file, `export_alias`, statement start/end bytes, exported name, source symbol, alias ordinal | `mod.rs:16915-16987` |
| `dispatch_hints.id` | file, `dispatch`, caller scoped symbol, line, byte start/end, dispatch ordinal | `mod.rs:16988-17026` |
| ordinary legacy `edges.edge_id` | full ref-id hex and `edge` | `mod.rs:17199-17224` |
| method-dispatch legacy edge | full ref-id hex, provenance text, `edge` | `mod.rs:20249-20290` |
| views ordinary ref | `view:` form tag, caller path, bound `ParseBlob::reference_keys` integer (not always AST ordinal) | `views/materialization.rs:1193`; `callgraph_store/join.rs:1062-1111` |
| views dispatch-site ref | same former `view:<path>:<key>` text, but key is `SiteHint.ordinal`, including sites without any BlobRef/refs row | `views/materialization/dispatch.rs:677` |
| views ordinary edge | `edge:view:` form tag, caller path, bound reference key | `views/materialization.rs:2087` |
| views dispatch edge | `dispatch:` form tag, site file, original site ordinal, target file, target scoped symbol; one identity per target | `views/materialization/dispatch.rs:863-868` |
| `staging_ref_context.ref_id` | exact referenced ref identity; its context retains caller symbol | `mod.rs:13256-13385`, resolve loop `5510-5537` |
| `emitted_view_rows.row_key` | typed key selected by `table_name`: file path for files, node id for nodes, ref id for refs, edge id for edges, dispatch-hint id for dispatch_hints | `views/materialization/stable_rows.rs:6-95` |
| other primary/composite keys | exact column tuples in `table_encoding.json.gz`; not new graph identities | schema statement ids in that file |

The original hash-kind is not interchangeable with `refs.kind`: construction
and callback-reference extraction can store a different kind. Caller symbol,
per-file ordinal, import index and original hash-kind need **new stored input
columns** even though the finished baseline does not retain all of them.
Views id constructors must hash their former complete text independently of
whether a refs row exists. Ordinary keys and dispatch ordinals cannot be
translated through each other when they happen to have the same spelling.

Macro readers are also information consumers: template-call lookup uses the
macro `local_name` when a caller node is absent, and unparseable macro mentions
are retained as distinct unresolved evidence rather than ordinary invocation
edges (`commands/callgraph_store_adapter.rs:790-864`). Keep both macro ref kinds,
their original hash-kind inputs and their full-ref/caller information.

## Text-sensitive readers

`source_inventory.json.gz:id_text_readers` is the line-by-line census of tests,
prefix checks, stripping and splitting around ids. It intentionally includes
test assertions and conservative context matches. The mappings below govern
those sites as well as the ordinary row decoders that merely return ids.

| Reader family | Information that must remain available |
| --- | --- |
| top-level exclusion / `top:` checks | stored node form tag; do not infer from signed integer sign after salting |
| `pos:symbol:` / `view:symbol:` checks and output assertions | stored node form tag plus node constructor inputs; reconstruct the complete original prefixed digest text |
| view-ref prefix/range reader and last-colon split in dispatch (`views/materialization/dispatch.rs:1122-1205`) | stored ref form, full caller path, original bound key or AST ordinal according to constructor; reconstruct complete text for range semantics, then validate file equality |
| view-edge map keyed by edge id (same reader) | edge form and every target component; dispatch and ordinary edge forms have different prefixes |
| resolver eager/lazy node/ref readers (`mod.rs:6568-6626`, `18946-19032`) | decoded ids and text; node scoped-name/id order and ref source-position/ref-id order unchanged |
| incremental resolver indexes (`mod.rs:19173-19227`) | same decoded identity and ordering as cold SQL, including first/last-wins selections |
| node lookup and row decoders (`mod.rs:11154-11264`, `11432-11447`) | kind, full scoped name, path, signature and id text; no lossy conversion at the API boundary |
| edge derivation in cold staging / dispatch / dependent refresh (`mod.rs:5510-5537`, `17211`, `20249-20290`) | full original ref digest from persistent identity inputs, never the integer prefix |
| stable-row emitted-key prune (`stable_rows.rs:77-95`) | table-specific typed key mapping, preserving equality and rowid rather than formatting a new key |

Every public reply that prints an id must print its former text. Strings used
only as cache keys still need equality-preserving decoding; they are not exempt
from collision checking. Id-keyed UPDATE/DELETE and foreign-reference writes
also require identity-input checks, notably a dispatch site's ref id with no
refs row (`dispatch.rs:884-923`).
