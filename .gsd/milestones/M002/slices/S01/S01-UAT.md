# S01: Import Management — UAT

**Milestone:** M002
**Written:** 2026-03-14

## UAT Type

- UAT mode: artifact-driven
- Why this mode is sufficient: All import management operations are deterministic transformations verifiable through the binary protocol's JSON responses. Integration tests exercise the full stdin→stdout round-trip. No UI, no human judgment needed.

## Preconditions

- `cargo build` succeeds with 0 warnings
- The `aft` binary is at `target/debug/aft`
- Test fixtures exist: `tests/fixtures/imports_ts.ts`, `imports_js.js`, `imports_py.py`, `imports_rs.rs`, `imports_go.go`
- `opencode-plugin-aft/` has `node_modules` installed (`bun install`)

## Smoke Test

Send `add_import` for a TS file via the binary protocol and confirm the response contains `ok: true`, `added: true`, `group: "external"`, and `syntax_valid: true`.

## Test Cases

### 1. add_import places TS import in correct external group

1. Create a TS file with 3 import groups: external (`react`), relative (`./utils`), and type (`import type { FC } from 'react'`)
2. Send `add_import` with `module: "lodash"`, `name: "debounce"`
3. **Expected:** Response has `added: true`, `group: "external"`. File content shows `import { debounce } from 'lodash';` in the external group (before the relative group), alphabetically ordered among existing external imports.

### 2. add_import places TS import in correct internal group

1. Use same TS file with existing groups
2. Send `add_import` with `module: "./helpers"`, `name: "format"`
3. **Expected:** Response has `added: true`, `group: "internal"`. Import appears in the relative/internal group section.

### 3. add_import deduplication

1. Use a TS file that already contains `import { useState } from 'react';`
2. Send `add_import` with `module: "react"`, `name: "useState"`
3. **Expected:** Response has `added: false`, `already_present: true`. File is unchanged on disk.

### 4. add_import alphabetizes within group

1. Use a TS file with `import { useState } from 'react';`
2. Send `add_import` with `module: "axios"`, `default: "axios"`
3. **Expected:** `axios` import appears before `react` import (alphabetical by module path).

### 5. add_import Python stdlib group

1. Create a Python file with `import json` (stdlib) and `import requests` (third-party)
2. Send `add_import` with `module: "os"`
3. **Expected:** Response has `group: "stdlib"`. `import os` appears in the stdlib group alongside `import json`.

### 6. add_import Python third-party group

1. Use same Python file
2. Send `add_import` with `module: "flask"`, `name: "Flask"`
3. **Expected:** Response has `group: "external"`. `from flask import Flask` appears in the third-party group.

### 7. add_import Python local group

1. Use a Python file with existing imports
2. Send `add_import` with `module: ".utils"`, `name: "helper"`
3. **Expected:** Response has `group: "internal"`. `from .utils import helper` appears in the local/internal group.

### 8. add_import Rust std group

1. Create a Rust file with `use serde::Deserialize;` (external)
2. Send `add_import` with `module: "std::collections::HashMap"`
3. **Expected:** Response has `group: "stdlib"`. `use std::collections::HashMap;` appears before the external imports.

### 9. add_import Rust external group

1. Use a Rust file with `use std::io;`
2. Send `add_import` with `module: "tokio::runtime::Runtime"`
3. **Expected:** Response has `group: "external"`. `use tokio::runtime::Runtime;` appears in the external group.

### 10. add_import Go stdlib group

1. Create a Go file with `import "github.com/gin-gonic/gin"` (external)
2. Send `add_import` with `module: "fmt"`
3. **Expected:** Response has `group: "stdlib"`. `import "fmt"` appears in the stdlib section.

### 11. add_import Go external group

1. Use a Go file with `import "fmt"`
2. Send `add_import` with `module: "github.com/stretchr/testify"`
3. **Expected:** Response has `group: "external"`. External import appears after stdlib.

### 12. add_import to empty file

1. Create an empty `.ts` file
2. Send `add_import` with `module: "react"`, `name: "useState"`
3. **Expected:** Response has `added: true`. File now contains `import { useState } from 'react';` as the first line.

### 13. remove_import entire statement

1. Create a TS file with `import { useState } from 'react';`
2. Send `remove_import` with `module: "react"`
3. **Expected:** Response has `removed: true`, `syntax_valid: true`. The import line is gone from the file.

### 14. remove_import specific name from multi-name import

1. Create a TS file with `import { useState, useEffect } from 'react';`
2. Send `remove_import` with `module: "react"`, `name: "useState"`
3. **Expected:** Response has `removed: true`. File now contains `import { useEffect } from 'react';` (useState removed, useEffect preserved).

### 15. remove_import last name removes entire statement

1. Create a TS file with `import { useState } from 'react';`
2. Send `remove_import` with `module: "react"`, `name: "useState"`
3. **Expected:** Entire import statement removed (not left as `import {} from 'react'`).

### 16. organize_imports re-sorts and re-groups TS

1. Create a TS file with imports in wrong order: relative first, then external, with some out of alphabetical order
2. Send `organize_imports`
3. **Expected:** Response has `groups` array showing group names and counts. File content shows external imports first (alphabetized), then relative imports (alphabetized), with a blank line between groups.

### 17. organize_imports deduplicates

1. Create a TS file with duplicate imports: `import { useState } from 'react';` appearing twice
2. Send `organize_imports`
3. **Expected:** Response has `removed_duplicates: 1` (or > 0). Only one `import { useState } from 'react';` remains.

### 18. organize_imports Python isort grouping

1. Create a Python file with imports in random order: local, then stdlib, then third-party
2. Send `organize_imports`
3. **Expected:** File content shows stdlib first, then third-party, then local — with blank lines between groups.

### 19. organize_imports Rust use-tree merging

1. Create a Rust file with separate `use std::path::Path;` and `use std::path::PathBuf;`
2. Send `organize_imports`
3. **Expected:** File contains `use std::path::{Path, PathBuf};` — merged into a single use-tree declaration.

### 20. Plugin tool registration round-trip

1. Run `bun test` in `opencode-plugin-aft/`
2. **Expected:** All 22 tests pass. Import tool definitions (add_import, remove_import, organize_imports) are registered with correct Zod schemas alongside existing tools.

## Edge Cases

### add_import on unsupported language

1. Create a `.txt` file
2. Send `add_import` with `module: "foo"`, `name: "bar"`
3. **Expected:** Response has `ok: false`, `code: "invalid_request"` with message indicating unsupported language.

### add_import on missing file

1. Send `add_import` with `file: "/nonexistent/path.ts"`, `module: "react"`, `name: "useState"`
2. **Expected:** Response has `ok: false`, `code: "file_not_found"`.

### add_import missing required params

1. Send `add_import` with `file` only — no `module`
2. **Expected:** Response has `ok: false`, `code: "invalid_request"` with message about missing params.

### remove_import on non-existent module

1. Create a TS file with `import { useState } from 'react';`
2. Send `remove_import` with `module: "nonexistent"`
3. **Expected:** Response has `ok: false`, `code: "import_not_found"`.

### Type import dedup independence

1. Create a TS file with `import { FC } from 'react';`
2. Send `add_import` with `module: "react"`, `name: "FC"`, `kind: "type"`
3. **Expected:** Response has `added: true` — type import does NOT dedup against value import of same name.

### Go dedup for grouped imports

1. Create a Go file with grouped import block containing `"fmt"`
2. Send `add_import` with `module: "fmt"`
3. **Expected:** Response has `already_present: true`. File unchanged.

## Failure Signals

- Any `cargo test -- import` failure indicates a broken import operation
- Response missing `syntax_valid` field on any mutation command
- `group` field returning unexpected values (not "stdlib", "external", or "internal")
- `bun test` failures indicate plugin registration broke — tools won't be visible to agents
- File content with empty import statements (e.g. `import {} from 'react'`) after remove_import

## Requirements Proved By This UAT

- R013 — Import management for all 6 languages: add_import (group placement, dedup, alphabetization), remove_import (full and partial), organize_imports (re-sort, re-group, dedup, Rust merge)
- R034 — Web-first priority verified: TS/JS/TSX share engine patterns, all languages covered

## Not Proven By This UAT

- Auto-formatting of import results (S03 — imports are correctly placed but not auto-formatted)
- Dry-run mode on import commands (S04)
- Transaction support across multiple files with import operations (S04)
- Cross-file import usage analysis (not in scope — M003/M004 territory)

## Notes for Tester

- All test cases can be run entirely through `cargo test -- import` and `cargo test --test integration` — the integration tests cover every scenario listed above.
- The `bun test` run in `opencode-plugin-aft/` validates plugin-side wiring. If it passes, the tools are correctly registered.
- Python stdlib detection uses a static list. If testing with a very new Python module added in 3.13+, it may classify as third-party.
- Go uses dot-in-path heuristic: `"fmt"` → stdlib, `"github.com/foo"` → external. There are no exceptions to check.
