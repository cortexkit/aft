#!/usr/bin/env python3
"""Render raw index-matrix results; missing observations stay empty, never zero."""
from __future__ import annotations

import argparse
from collections import defaultdict
import csv
import json
import math
from pathlib import Path

HERE = Path(__file__).resolve().parent
LANGUAGES = {name.lower(): name for name in ("Java", "Scala", "TypeScript", "TSX", "JavaScript", "Python", "Go", "Rust", "Solidity", "Ruby", "Kotlin", "Swift", "PHP", "Perl", "Lua", "C", "C++", "CUDA", "Metal", "Zig", "C#")}
LANGUAGES.update({"cpp": "C++", "csharp": "C#", "c_sharp": "C#"})


def percentile(values, percent):
    """Nearest-rank percentile, preserving the input units and integer values."""
    if not values:
        return None
    return sorted(values)[max(0, math.ceil(len(values) * percent / 100) - 1)]


def write_csv(path, rows, fields=None):
    fields = fields or list(dict.fromkeys(key for row in rows for key in row))
    with path.open("w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=fields, lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)


def markdown(rows, fields):
    def value(row, key):
        item = row.get(key)
        return "not measured" if item is None else str(item).replace("|", "\\|").replace("\n", " ")
    lines = ["| " + " | ".join(fields) + " |", "| " + " | ".join("---" for _ in fields) + " |"]
    lines.extend("| " + " | ".join(value(row, key) for key in fields) + " |" for row in rows)
    return "\n".join(lines)


def generate(manifest, results, output):
    summaries, shapes, latencies = [], [], []
    for entry in manifest["repositories"]:
        repo = entry["repository"]
        directory = results / repo.replace("/", "--")
        path = directory / "result.json"
        result = json.loads(path.read_text()) if path.exists() else {}
        row = {"repository": repo, "commit": entry["commit"], "checkout_bytes": result.get("checkout_bytes"),
               "checkout_allocated_bytes": result.get("checkout_allocated_bytes")}
        for mode in ("text", "meaning", "full"):
            build = result.get(mode, {})
            row[mode + "_outcome"] = build.get("outcome", "not_measured")
            for metric in ("vmhwm_bytes", "vmhwm_observed_bytes", "cgroup_memory_peak_bytes", "oom_killed", "wall_ns", "cpu_s", "store_bytes", "store_allocated_bytes", "storage_bytes", "model_bytes"):
                row[mode + "_" + metric] = build.get(metric)
            row[mode + "_reason"] = build.get("reason")
            failures = build.get("semantic_failures", {})
            row[mode + "_files_failed"] = failures.get("files_failed")
            row[mode + "_reported_failed_file_names"] = json.dumps(failures.get("reported_file_names")) if failures else None
            row[mode + "_census_observations"] = build.get("census_observations")
            for component, value in build.get("census_observed_max_bytes", {}).items():
                row[mode + "_census_observed_max_bytes_" + component] = value
        shape_status = result.get("call_shape", {"outcome": "not_measured", "reason": "repository not run"})
        query_status = result.get("queries", {"outcome": "not_measured", "reason": "repository not run"})
        row["call_shape_outcome"] = shape_status["outcome"]
        row["call_shape_reason"] = shape_status.get("reason")
        row["query_outcome"] = query_status["outcome"]
        row["query_reason"] = query_status.get("reason")
        groups = defaultdict(list)
        language_totals = {}
        if shape_status["outcome"] == "measured":
            artifact = json.loads((directory / shape_status["artifact"]).read_text())
            for total in artifact.get("language_totals", []):
                language_totals[LANGUAGES.get(total["language"], total["language"])] = total
            for group in artifact["groups"]:
                groups[LANGUAGES.get(group["language"], group["language"])].append(group)
        for language in sorted(set(entry["languages"]) | set(groups)):
            measured = shape_status["outcome"] == "measured"
            sizes = [group["sites"] for group in groups[language]]
            targets = [group["targets"] for group in groups[language]]
            total = language_totals.get(language)
            shape = {"repository": repo, "language": language, "outcome": shape_status["outcome"],
                     "all_method_sites": total["sites"] if total else None,
                     "unknown_exit_sites": sum(count for exit_name, count in total["exits"].items() if exit_name.startswith("unknown")) if total else None,
                     "unknown_sites": sum(sizes) if measured else None,
                     "distinct_language_member_groups": len(sizes) if measured else None,
                     "sites_per_group_p50": percentile(sizes, 50), "sites_per_group_p99": percentile(sizes, 99),
                     "sites_per_group_max": max(sizes) if sizes else None,
                     "targets_per_group_p50": percentile(targets, 50), "targets_per_group_p99": percentile(targets, 99),
                     "targets_per_group_max": max(targets) if targets else None,
                     "reason": shape_status.get("reason")}
            shapes.append(shape)
        observations = defaultdict(list)
        if query_status["outcome"] == "measured":
            with (directory / result["attempt"] / "full/queries.jsonl").open() as handle:
                for line in handle:
                    sample = json.loads(line)
                    observations[sample["op"], sample["temperature"]].append(sample["latency_ns"])
        for operation in ("callers", "impact", "trace_to"):
            for temperature in ("cold", "warm"):
                values = observations[operation, temperature]
                latency = {"repository": repo, "operation": operation, "temperature": temperature,
                           "samples": len(values), "p50_ns": percentile(values, 50),
                           "p95_ns": percentile(values, 95), "p99_ns": percentile(values, 99),
                           "outcome": query_status["outcome"], "reason": query_status.get("reason")}
                latencies.append(latency)
                for percent in (50, 95, 99):
                    row[f"{operation}_{temperature}_p{percent}_ns"] = latency[f"p{percent}_ns"]
        summaries.append(row)
    output.mkdir(parents=True, exist_ok=True)
    write_csv(output / "repositories.csv", summaries)
    write_csv(output / "call-shape.csv", shapes)
    write_csv(output / "query-latency.csv", latencies)
    (output / "tables.json").write_text(json.dumps({"repositories": summaries, "call_shape": shapes, "query_latency": latencies}, indent=2) + "\n")
    sections = ["# Index matrix measurements", "Missing values mean not measured. Times are integer nanoseconds (CPU: kernel seconds); memory and disk are integer bytes. Percentiles use nearest rank. Call-shape rows are per repository/language, not sums of overlapping project namespaces.",
                "## Cold text and meaning indexes", markdown(summaries, ["repository", "text_outcome", "text_vmhwm_bytes", "text_wall_ns", "text_cpu_s", "text_store_bytes", "meaning_outcome", "meaning_vmhwm_bytes", "meaning_wall_ns", "meaning_cpu_s", "meaning_store_bytes"]),
                "## Call shape", markdown(shapes, ["repository", "language", "outcome", "unknown_sites", "distinct_language_member_groups", "sites_per_group_p50", "sites_per_group_p99", "sites_per_group_max", "reason"]),
                "## Query latency", markdown(latencies, ["repository", "operation", "temperature", "outcome", "samples", "p50_ns", "p95_ns", "p99_ns", "reason"])]
    (output / "tables.md").write_text("\n\n".join(sections) + "\n")
    return len(summaries), len(shapes), len(latencies)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=HERE / "manifest.json")
    parser.add_argument("--results", type=Path, default=HERE.parents[2] / "target/index-matrix/results")
    parser.add_argument("--output", type=Path, help="defaults to <results>/tables")
    args = parser.parse_args()
    counts = generate(json.loads(args.manifest.read_text()), args.results, args.output or args.results / "tables")
    print(f"wrote {counts[0]} repository rows, {counts[1]} language rows, {counts[2]} latency rows")


if __name__ == "__main__":
    main()
