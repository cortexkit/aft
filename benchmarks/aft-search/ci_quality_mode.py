#!/usr/bin/env python3
"""Select the narrow manifest/bundle verification exception from one full diff."""
from __future__ import annotations
import argparse, json, subprocess, sys
from pathlib import Path

def select(paths:list[str])->str:
    manifest="benchmarks/aft-search/real-query-manifest.json"
    allowed=lambda path: path==manifest or path.startswith("benchmarks/aft-search/bundles/")
    return "verify" if manifest in paths and paths and all(allowed(path) for path in paths) else "evaluate"

def main()->int:
    p=argparse.ArgumentParser(); p.add_argument("--base"); p.add_argument("--head",default="HEAD"); p.add_argument("--paths",nargs="*"); p.add_argument("--self-test",action="store_true"); args=p.parse_args()
    if args.self_test:
        assert select(["benchmarks/aft-search/real-query-manifest.json"])=="verify"
        assert select(["benchmarks/aft-search/real-query-manifest.json","benchmarks/aft-search/bundles/a.zip"])=="verify"
        assert select(["benchmarks/aft-search/real-query-manifest.json","benchmarks/aft-search/real-query-baseline.json","benchmarks/aft-search/manifest.sha256"])=="evaluate"
        assert select(["benchmarks/aft-search/search_quality.py"])=="evaluate"
        print("ci_quality_mode_goldens:ok"); return 0
    paths=args.paths
    if paths is None:
        if not args.base: print("unresolvable_diff",file=sys.stderr); return 2
        result=subprocess.run(["git","diff","--name-only",f"{args.base}...{args.head}"],text=True,capture_output=True,check=False)
        if result.returncode: print("unresolvable_diff:"+result.stderr,file=sys.stderr); return 2
        paths=result.stdout.splitlines()
    print(select(paths)); return 0
if __name__=="__main__": raise SystemExit(main())
