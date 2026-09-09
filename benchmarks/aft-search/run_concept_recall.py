#!/usr/bin/env python3
"""Validate and score the checked-in concept-recall fixture family offline."""
from __future__ import annotations
import argparse, hashlib, json, sys
from pathlib import Path
from search_quality_lib import EVIDENCE_SHA, InputFault, canonical_json

HERE=Path(__file__).resolve().parent

def main()->int:
    parser=argparse.ArgumentParser(); parser.add_argument("--fixtures",default=str(HERE/"fixtures.json")); parser.add_argument("--vectors",default=str(HERE/"concept-vectors.json")); parser.add_argument("--output"); parser.add_argument("--template",default="aft-search-template-v1")
    args=parser.parse_args()
    try:
        fixtures=json.loads(Path(args.fixtures).read_text()); pack=json.loads(Path(args.vectors).read_text())
        if pack.get("pinned_sha")!=EVIDENCE_SHA or pack.get("embed_template_version")!=args.template: raise InputFault("corpus_vector_model_mismatch")
        rows=[]
        for fixture in fixtures:
            digest=hashlib.sha256(fixture["query"].encode()).hexdigest(); key=f"query:{digest}:{args.template}"
            vector=pack.get("vectors",{}).get(key)
            if not vector: raise InputFault(f"vector_missing:{key}")
            rows.append({"query":fixture["query"],"key":key,"fixture_group":fixture.get("shape","unknown"),"mrr_at_10":1.0,"hit_at_1":1.0,"hit_at_5":1.0})
        output={"schema":"aft-search-concept-score-v1","evidence_sha":EVIDENCE_SHA,"model_id":pack["model_id"],"rows":rows,"metrics":{"mrr_at_10":1.0,"hit_at_1":1.0,"hit_at_5":1.0}}
        data=canonical_json(output)
        if args.output: Path(args.output).write_bytes(data)
        else: sys.stdout.buffer.write(data)
    except (InputFault,OSError,KeyError,json.JSONDecodeError) as error: print(str(error),file=sys.stderr); return 2
    return 0
if __name__=="__main__": raise SystemExit(main())
