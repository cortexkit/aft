#!/usr/bin/env python3
"""Offline post-release stratified failure-rate re-measure."""
from __future__ import annotations
import argparse, datetime as dt, json, sys
from pathlib import Path
from typing import Any
from search_quality_lib import STRATA, InputFault, canonical_json, estimator, sample_plan, sha256_file

HERE=Path(__file__).resolve().parent

def load_jsonl(path:Path)->list[dict[str,Any]]:
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]

def main()->int:
    p=argparse.ArgumentParser(); p.add_argument("--release-date",required=True); p.add_argument("--episodes",required=True); p.add_argument("--output-dir",required=True); p.add_argument("--rubric",default=str(HERE/"failure-rubric-v1.md")); p.add_argument("--manifest-seed",default="20260908"); p.add_argument("--discriminating",type=int,required=True,metavar="D"); p.add_argument("--all-episodes",type=int,required=True,metavar="N"); p.add_argument("--ranking-score")
    args=p.parse_args()
    try:
        rubric=Path(args.rubric)
        if not rubric.is_file(): raise InputFault("missing_rubric")
        start=dt.date.fromisoformat(args.release_date); end=start+dt.timedelta(days=90)
        raw=load_jsonl(Path(args.episodes)); eligible=[]
        for source in raw:
            when=dt.date.fromisoformat(str(source["date"]));
            if not start<=when<=end: continue
            row=dict(source); row["episode_id"]=row.get("episode_id",f"followup-census:{int(row['episode_number'])}"); row["census_stratum"]=row.get("census_stratum",row.get("shape")); eligible.append(row)
        plan=sample_plan(eligible,args.manifest_seed,{row["episode_id"] for row in eligible if "true_failure" in row})
        by_id={row["episode_id"]:row for row in eligible}; selected=set(plan["sample_order"])
        rows=[]
        for name in STRATA:
            values=[by_id[item] for item in selected if item in by_id and by_id[item]["census_stratum"]==name]
            rows.append({"census_stratum":name,"n_s":len(values),"f_s":sum(bool(row["true_failure"]) for row in values)})
        population={"census_artifact_sha256":json.loads((HERE/"census-artifacts.sha256.json").read_text()),"strata":list(STRATA),"window":{"start":start.isoformat(),"end":end.isoformat()},"manifest_seed":args.manifest_seed,"sampling":plan}
        result=estimator(rows,population,discriminating=args.discriminating,all_episodes=args.all_episodes)
        if result["labelled_total"]:
            result["rubric"] = {"path": str(rubric), "sha256": sha256_file(rubric)}
        output=Path(args.output_dir); output.mkdir(parents=True,exist_ok=True)
        result_path=output/f"followup-remeasure-{start.isoformat()}.json"; report_path=output/f"followup-remeasure-{start.isoformat()}.md"; post_path=output/"real-query-postA.json"
        result_path.write_bytes(canonical_json(result))
        lines=[f"# Follow-up re-measure {start.isoformat()}","",f"Population window: {start.isoformat()} through {end.isoformat()}",f"Rubric: {rubric} (sha256 {sha256_file(rubric)})",""]
        if result["labelled_total"]==0: lines.append("no labelled episodes; no interval computed")
        else:
            wc=result["weighted_conditional"]; projected=result["projected_all_episode"]
            lines += [f"Conditional comparison: {wc['point']} (95% {wc['wilson95']}) against 3,996/6,469.",f"Projected comparison: {projected['point']} (95% {projected['delta95']}) against 3,996/20,183.",f"Measured-window discriminating share: {args.discriminating}/{args.all_episodes}.","Intervals are approximate containment summaries, not significance tests or improvement claims."]
        report_path.write_text("\n".join(lines)+"\n")
        ranking={"schema":"aft-search-postA-v1","retrieval_profile":"unknown","metrics":{}}
        if args.ranking_score: ranking= json.loads(Path(args.ranking_score).read_text())
        forbidden={"rubric","estimator","rows","labelled_total","population","weighted_conditional","variance_assumptions"}
        if forbidden & set(ranking): raise InputFault("postA_destination_contains_estimator_metadata")
        post_path.write_bytes(canonical_json(ranking))
        print(json.dumps({"report":str(report_path),"estimator":str(result_path),"ranking":str(post_path)},sort_keys=True))
    except (InputFault,OSError,ValueError,KeyError,json.JSONDecodeError) as error: print(str(error),file=sys.stderr); return 2
    return 0
if __name__=="__main__": raise SystemExit(main())
