#!/usr/bin/env python3
"""Reproduce query/pattern fixtures from census rows in real-query-manifest.json."""
from __future__ import annotations
import json
from pathlib import Path
from search_quality_lib import canonical_json

HERE = Path(__file__).resolve().parent
MAGIC_CONTEXT_SOURCE = (
    "constructed from pinned repository content after a magic-context aft_search report "
    "(query 'Pi auto-search hint selects latest user message to search on; skips synthetic or custom messages', "
    "pattern 'autoSearch|auto_search|runAutoSearch'): that repository is not a benchmark corpus, so the row "
    "rebuilds the same shape on the pin; not telemetry"
)


def main() -> None:
    path = HERE / "real-query-manifest.json"
    manifest = json.loads(path.read_text())
    manifest["rows"] = [row for row in manifest["rows"] if "split_kind" not in row]
    original = {row["episode_id"]: row for row in manifest["rows"]}
    template = original["followup-census:7614"]
    fields = ("repo", "sha", "embedding_pack", "embedding_pack_sha256", "evidence_tree_sha256")

    def row(number: int, kind: str, query: str, pattern: str, answer: str, basis: str, source: str = "constructed from pinned repository content; no census counterpart", answer_kind: str = "concept", **extra: object) -> dict:
        return {**{key: template[key] for key in fields}, "episode_id": f"followup-census:{number}",
                "split_kind": kind, "query": query, "pattern": pattern, "opened_file": answer,
                "answer_kind": answer_kind, "row_source": source, "answer_key_basis": basis,
                "include_tests": False, "include_tests_source": "default", "pinned_shape": "split",
                "mechanism": "split_query_pattern_fusion", "census_stratum": "nl", **extra}

    def census(number: int, kind: str, episode: int, query: str, pattern: str, basis: str, **extra: object) -> dict:
        old = original[f"followup-census:{episode}"]
        return row(number, kind, query, pattern, old["opened_file"], basis,
                   f"hand split of retained followup-census:{episode}: {old['query']}", **extra)

    names = "FormatContext HostEscalationAttempt PinOwner ServerKey LspManager LspChildRegistry FileId InspectSnapshot DispatchHandles SwiftSyntax PhpSyntax KotlinSyntax CSharpSyntax CSyntax ExternalToolResult WriteResult FileFreshness AlertEngine SemanticIndexFingerprint SearchIndex ShapeWeights ListEnvelope LockGuard AppContext Config TomlFilter PytestCompressor CargoCompressor BunCompressor OutputProbe".split()
    thirty = r"^pub struct (" + "|".join(names) + r")\b"
    shutdown_basis = original["followup-census:7957"]["answer_key_basis"]
    rows = [
        census(910001, "R1", 7614, "how does structural search parallelize work across files", "ast_grep_search", "At the pin ast_search.rs:146-149 uses rayon par_iter. The old public name occurs only as a mention in ast_grep_hints.rs:80 among non-test Rust sources."),
        row(910002, "R1", "how are background bash tasks restored after restart", "subagent_type", "crates/aft/src/bash_background/registry.rs", "At the pin registry.rs restores persisted background tasks; subagent_type is a stale name confined to query_shape_test.rs:47 (includeTests true for this row), not a declaration.", include_tests=True),
        census(910003, "R2", 7957, "what does the subc module wait for before exiting after shutdown", "Error|Result", shutdown_basis),
        census(910004, "R2", 3184, "how does patch write files with durability and backup", "Error|Result", "At the pin hashline/snapshot/mod.rs coordinates durable snapshot writes, sync and backup ownership; this is the retained census opened file."),
        row(910005, "R3", "how is command output compressed with filters and probes", thirty, "crates/aft/src/compress/mod.rs", "At the pin compress/mod.rs:109 declares OutputProbe and :165 CompressionResult; the anchored alternation selects declarations in exactly 30 source files.", pattern_files=30, expected_prose_overlap=5, prose_overlap_basis="Informational expectation: output compression concerns OutputProbe, TomlFilter, PytestCompressor, CargoCompressor and BunCompressor; other declaration files cover LSP, imports, indexing, locks and health. Actual lane membership is reported by the split engine, not asserted as an exact count."),
        row(910006, "R4", "what does the subc module wait for before exiting after shutdown", "shutdown_all", "crates/aft/src/lsp/manager.rs", "At the pin lsp/manager.rs:2295 defines shutdown_all, the LSP shutdown invoked by the module exit sequence.", answer_kind="definition", pair_id="shutdown"),
        census(910007, "R4", 7957, "what does the subc module wait for before exiting after shutdown", "shutdown_all", shutdown_basis, pair_id="shutdown"),
        census(910008, "R5", 7614, "how does structural search parallelize work across files", "", "At the pin ast_search.rs:146-149 imports rayon and calls par_iter; empty pattern must preserve the query-only result."),
        census(910009, "R5", 7957, "what does the subc module wait for before exiting after shutdown", "   ", shutdown_basis),
        census(910010, "R6", 7614, "how does structural search parallelize work across files", "handle_ast_search", "At the pin ast_search.rs:24 defines handle_ast_search and :146-149 fans work out with rayon. Index embeddings are held while lexical indexing completes.", answer_kind="definition", semantic_state="building"),
        census(910011, "R7", 7670, "perf tier2 phases freshness snapshot scan db rollup log emit", "Tier2PhaseTimings|Error", "At the pin inspect/manager.rs orchestrates tier2 phases and records Tier2PhaseTimings; this is the retained census opened file."),
        census(910012, "R7", 7656, "status snapshot session id checkpoints json serialization", "tracked_files|Error", "At the pin commands/status.rs:78-82 builds the status snapshot; the retained census asks for its tracked_files and session serialization."),
        # Rebuilt from a search in the magic-context repository (see
        # MAGIC_CONTEXT_SOURCE): a sentence containing a comma or semicolon,
        # and a pattern alternating camelCase, snake_case and a longer name,
        # where the answer file declares a name that only begins with one
        # alternative (runAutoSearch -> runAutoSearchHintForPi). The rows use
        # names of the same shape that exist on the pin.
        row(910013, "R7", "Pi plugin resolves the user and project aft config file paths, migrating legacy config files first", "resolveAft|resolve_aft|resolveAftConfig", "packages/pi-plugin/src/config.ts", "At the pin packages/pi-plugin/src/config.ts:1764 declares resolveAftConfigPaths, which resolves the user and project config paths and migrates legacy config files first (migrateLegacyAftConfigFiles, from line 1741); opencode-plugin/src/config.ts:1789 declares the OpenCode twin. No file declares resolveAft or resolveAftConfig as a whole name.", source=MAGIC_CONTEXT_SOURCE, answer_kind="definition"),
        row(910014, "R7", "how are call edges resolved across files when the callee is imported, re-exported or aliased", "resolve_cross_file|resolveCrossFile|crossFileEdge", "crates/aft/src/callgraph.rs", "At the pin crates/aft/src/callgraph.rs:1120 declares resolve_cross_file_edge and :942 resolve_cross_file_edge_with_exports, which follows re-exports through resolve_reexported_symbol (lines 991-1059). resolve_cross_file is mentioned in 18 Markdown files and declared nowhere as a whole name.", source=MAGIC_CONTEXT_SOURCE, answer_kind="definition"),
    ]
    manifest["rows"].extend(rows)
    path.write_bytes(canonical_json(manifest))
    tuning = [
        census(920001, "R2", 7956, "profile sample file removed on symbolization failure dsym download error decoding", "remove_file|Error", "At the pin cli/profile.rs removes profile samples after symbolization failure; retained census opened file."),
        census(920002, "R7", 7695, "install standalone push frame stdout closure wiring", "set_progress_sender|Result", "At the pin main.rs installs the progress sender and emits standalone frames; retained census opened file."),
        census(920003, "R2", 4212, "implementation of formatter skip reasons and skipped count", "format_skip_reasons|Result", "At the pin commands/edit_match.rs:670-798 aggregates skipped reasons and returns the count."),
        census(920004, "R7", 17208, "health suspended doctor reset", "build_status_snapshot|Error", "At the pin commands/configure.rs produces configure health and reset responses; retained census opened file."),
        census(920005, "R5", 16765, "relative absolute path serialize portability", "", "At the pin search_index.rs handles canonical and relative search paths; retained census opened file."),
        census(920006, "R4", 7972, "test harness spawns a fake daemon then drops the socket and asserts the module thread result", "run_subc_mode_for_test", original["followup-census:7972"]["answer_key_basis"], include_tests=True),
        census(920007, "R7", 14964, "stale diagnostics", "mark_file_diagnostics_stale|Error", "The retained stale identifier query opens context.rs; the pin's diagnostic invalidation coordination is in that file."),
        census(920008, "R2", 7958, "Shutdown control request handler sequence after receiving shutdown and bounded timeout on each step", "drain_on_shutdown|Result", original["followup-census:7958"]["answer_key_basis"]),
        # Concept answers that call the named symbol (tuning rows for the
        # split-query call-graph bridge).
        row(920009, "R4", "terminate background bash tasks of a project root after its directory disappears, before artifact eviction", "kill_running_tasks_for_root", "crates/aft/src/subc/mod.rs", "At the pin bash_background/registry.rs:3205 defines kill_running_tasks_for_root; subc/mod.rs:1383 calls it when a deleted root is reclaimed, the concept the query describes.", answer_relation="caller"),
        row(920010, "R4", "drain completions reply returns the pending watch pattern matches for the session", "pending_pattern_matches_for_session", "crates/aft/src/commands/bash_drain_completions.rs", "At the pin bash_background/registry.rs:971 defines pending_pattern_matches_for_session; commands/bash_drain_completions.rs:19 calls it to build the drain reply.", answer_relation="caller"),
        row(920011, "R4", "health rollup publishes the active build suspensions read from the breaker", "active_suspensions_for_root_at|Error", "crates/aft/src/subc/health.rs", "At the pin build_breaker.rs:403 defines active_suspensions_for_root_at; subc/health.rs:875 calls it in the health rollup and publishes the suspensions.", answer_relation="caller"),
    ]
    for item in tuning:
        item["tuning_only"] = True
    tuning_path = HERE / "split-tuning-manifest.json"
    tuning_manifest = {"schema": manifest["schema"], "evidence_sha": manifest["evidence_sha"], "tuning_only": True, "rows": tuning}
    # The manifest's split_query_pack entry names the vector pack (path and
    # digest) that holds the tuning rows' query vectors; only the
    # --tuning-only vector capture writes it. Carry it over, or the
    # regenerated manifest would point at no vectors and the tuning replay
    # would refuse its rows.
    if tuning_path.is_file() and "split_query_pack" in (previous := json.loads(tuning_path.read_text())):
        tuning_manifest["split_query_pack"] = previous["split_query_pack"]
    tuning_path.write_bytes(canonical_json(tuning_manifest))
    print("gate:14 R1=2 R2=2 R3=1 R4=2 R5=2 R6=1 R7=4; tuning:11")


if __name__ == "__main__":
    main()
