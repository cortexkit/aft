#!/usr/bin/env python3
"""Mechanically expand BQ0's extglob fence and audit its actual diff."""
from __future__ import annotations
import argparse,json,subprocess,sys
from pathlib import Path

ROOT=Path(__file__).resolve().parents[2]
SCRIPTS={"scripts/telemetry/cost-gate.sh","scripts/telemetry/imports-resolve.sh","scripts/telemetry/mutate.sh","scripts/telemetry/postA-remeasure.sh"}
WORKFLOW=".github/workflows/tests.yml"
EXCLUDED=("benchmarks/aft-search/engine-fixtures/","crates/aft/src/query_shape.rs","crates/aft/src/semantic_index.rs","crates/aft/src/embed/","crates/aft/src/lib.rs","crates/aft/src/commands/semantic_search.rs","crates/aft/src/commands/semantic_search/")

def in_benchmark(path:str)->bool: return path.startswith("benchmarks/aft-search/") and not path.startswith("benchmarks/aft-search/engine-fixtures/")
def in_slice(path:str)->bool: return (in_benchmark(path) and path not in {"benchmarks/aft-search/real-query-baseline.json","benchmarks/aft-search/manifest.sha256"}) or path in SCRIPTS or path==WORKFLOW

def main()->int:
 p=argparse.ArgumentParser(); p.add_argument("--base"); p.add_argument("--cached",action="store_true"); p.add_argument("--paths",nargs="*"); args=p.parse_args()
 paths=args.paths
 if paths is None:
  command=["git","diff","--name-only"]
  if args.cached: command.append("--cached")
  elif args.base: command.append(f"{args.base}...HEAD")
  result=subprocess.run(command,cwd=ROOT,text=True,capture_output=True,check=False)
  if result.returncode: print("fence_diff_unresolvable",file=sys.stderr); return 2
  paths=result.stdout.splitlines()
 bad=sorted(path for path in paths if not in_slice(path)); intersections=sorted(path for path in paths if any(path==prefix or path.startswith(prefix) for prefix in EXCLUDED))
 repository_listing=subprocess.run(["git","ls-files","--cached","--others","--exclude-standard","benchmarks/aft-search"],cwd=ROOT,text=True,capture_output=True,check=True).stdout.splitlines()
 benchmark_paths={path for path in repository_listing if not path.startswith("benchmarks/aft-search/engine-fixtures/")}
 expanded={path for path in benchmark_paths if path not in {"benchmarks/aft-search/real-query-baseline.json","benchmarks/aft-search/manifest.sha256"}}|SCRIPTS|{WORKFLOW}
 union_ok=all(in_slice(path) for path in expanded) and not any(path.startswith("benchmarks/aft-search/engine-fixtures/") for path in expanded)
 result={"changed_paths":paths,"outside_slice":bad,"sibling_intersections":intersections,"expanded_count":len(expanded),"pairwise_disjoint":not intersections,"union_equal_exhaustive_minus_b0_pair":union_ok}
 print(json.dumps(result,sort_keys=True))
 return 2 if bad or intersections or not union_ok else 0
if __name__=="__main__": raise SystemExit(main())
