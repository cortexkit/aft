#!/usr/bin/env python3
"""Network-enabled authoring command; never selected by the CI quality gate."""
from __future__ import annotations
import argparse, hashlib, json, sys, urllib.request
from pathlib import Path
from search_quality_lib import EVIDENCE_SHA, InputFault, canonical_json

def main()->int:
    parser=argparse.ArgumentParser(); parser.add_argument("--endpoint",required=True); parser.add_argument("--inputs",required=True); parser.add_argument("--output",required=True); parser.add_argument("--model",required=True); parser.add_argument("--template",required=True); parser.add_argument("--allow-network-authoring",action="store_true",required=True)
    args=parser.parse_args()
    try:
        inputs=json.loads(Path(args.inputs).read_text()); vectors={}
        for row in inputs:
            text=str(row["text"]); kind=row["kind"]
            body=json.dumps({"model":args.model,"input":text}).encode(); request=urllib.request.Request(args.endpoint.rstrip("/")+"/v1/embeddings",data=body,headers={"content-type":"application/json"})
            with urllib.request.urlopen(request,timeout=60) as response: vector=json.load(response)["data"][0]["embedding"]
            digest=hashlib.sha256(text.encode()).hexdigest()
            key=f"query:{digest}:{args.template}" if kind=="query" else f"corpus:{EVIDENCE_SHA}:{digest}:{args.template}"
            if not vector: raise InputFault(f"vector_missing:{key}")
            vectors[key]=vector
        Path(args.output).write_bytes(canonical_json({"schema":"aft-search-vector-pack-v1","pinned_sha":EVIDENCE_SHA,"embed_template_version":args.template,"model_id":args.model,"vectors":vectors}))
    except (OSError,KeyError,json.JSONDecodeError,InputFault) as error: print(str(error),file=sys.stderr); return 2
    return 0
if __name__=="__main__": raise SystemExit(main())
