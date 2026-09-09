#!/usr/bin/env python3
"""CLI and executable goldens for the B1 search-quality predicate."""
from __future__ import annotations

import argparse, copy, hashlib, json, os, subprocess, sys, tempfile
from pathlib import Path
from typing import Any

from search_quality_lib import (
    EVIDENCE_SHA, GateResult, InputFault, STRATA, atomic_write_pair, blake3,
    canonical_json, choose_stop, derive_slice_class, estimator, identity_delta,
    included_manifest_ids, invariance_requests, profile_requests, row_metrics,
    sample_plan, sha256_bytes, sha256_file, total_gate, validate_profile_score,
    validate_scored_population,
)

ROOT = Path(__file__).resolve().parents[2]
BENCH = Path(__file__).resolve().parent


def read_json(path: Path) -> dict[str, Any]:
    try: value=json.loads(path.read_text())
    except FileNotFoundError as error: raise InputFault(f"missing_input:{path}") from error
    except json.JSONDecodeError as error: raise InputFault(f"malformed_schema:{path}") from error
    if not isinstance(value,dict): raise InputFault(f"malformed_schema:{path}")
    return value


def diff_paths(base: str | None, head: str) -> list[str]:
    if not base: return []
    result=subprocess.run(["git","diff","--name-only",f"{base}...{head}"],cwd=ROOT,text=True,capture_output=True,check=False)
    if result.returncode: raise InputFault(f"unresolvable_diff:{result.stderr.strip()}")
    return [line for line in result.stdout.splitlines() if line]


def descriptor_path(explicit: str | None, branch: str | None) -> Path | None:
    if explicit: return Path(explicit)
    if not branch:
        result=subprocess.run(["git","branch","--show-current"],cwd=ROOT,text=True,capture_output=True,check=False); branch=result.stdout.strip()
    candidate=BENCH/"slice-descriptors"/f"{branch}.json"
    return candidate if candidate.is_file() else None


def binding(reference_path: Path, manifest_path: Path, sidecar_path: Path) -> dict[str, Any]:
    if not reference_path.is_file(): raise InputFault("missing_reference")
    if not sidecar_path.is_file(): raise InputFault("reference_manifest_mismatch:repair=record-reference --manifest-changed")
    sidecar=read_json(sidecar_path)
    if sidecar.get("manifest_sha256") != sha256_file(manifest_path) or sidecar.get("reference_sha256") != sha256_file(reference_path):
        raise InputFault("reference_manifest_mismatch:repair=record-reference --manifest-changed")
    return sidecar


def sidecar_bytes(manifest_path: Path, reference_bytes: bytes) -> bytes:
    return canonical_json({"schema":"aft-search-reference-binding-v1","manifest_sha256":sha256_file(manifest_path),"reference_sha256":sha256_bytes(reference_bytes)})


def synthetic_documents() -> tuple[dict[str,Any],dict[str,Any],dict[str,Any]]:
    manifest={"schema":"manifest","rows":[{"episode_id":"followup-census:1","include_tests":False,"include_tests_source":"default"}]}
    row={"episode_id":"followup-census:1","request":{"includeTests":False,"topK":100},"include_tests_source":"default","pages_fetched":1,"collapse_stop_reason":"exhausted"}
    metrics={"mrr_at_10":0.5,"hit_at_1":0.5,"hit_at_5":0.8}
    reference={"schema":"aft-search-score-v1","model_id":"fixture","profile":"single_page","capability":{"schema_path":"fixture.json","schema_sha256":"0"*64,"offset_declared":False},"families":{"exact_recall":dict(metrics),"concept_recall":dict(metrics),"real_query":dict(metrics)},"fixture_groups":{"exact_recall":{"g":dict(metrics)},"concept_recall":{"g":dict(metrics)}},"shapes":{"identifier":dict(metrics)},"mechanisms":{"topk_cut":dict(metrics)},"rows":[dict(row)]}
    score=copy.deepcopy(reference); score["rows"]=[dict(row)]; score["fixture_results"]={"harness-goldens":True,"paging":True}
    return manifest,reference,score


def self_test() -> None:
    assert blake3(b"").hex()=="af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
    assert row_metrics(["wrong.rs::a","wrong.rs::b","wrong.rs::c","opened.rs::x"],"opened.rs")=={"mrr_at_10":0.5,"hit_at_1":0.0,"hit_at_5":1.0}
    assert row_metrics([f"p/{i}" for i in range(11)]+["label"],"label")["mrr_at_10"]==0
    assert choose_stop(page_cap=True,exhausted=True,ten_files=True)=="page_cap"
    assert choose_stop(page_cap=False,exhausted=True,ten_files=True)=="exhausted"
    assert profile_requests("single_page",False)==[{"topK":100}]
    assert [len(plan) for plan in invariance_requests()]==[10,4,1]
    try: profile_requests("paged",False); raise AssertionError("paged without offset passed")
    except InputFault: pass
    population=[]
    for si,name in enumerate(STRATA):
        for number in range(1,4+si): population.append({"episode_id":f"followup-census:{si*100+number}","census_stratum":name})
    first=sample_plan(population,"imbalanced-golden"); second=sample_plan(population,"imbalanced-golden")
    assert canonical_json(first)==canonical_json(second)
    golden=read_json(BENCH/"goldens"/"imbalanced-sample.json")
    assert first==golden
    manifest,reference,score=synthetic_documents()
    nonranking={"slice_class":"non_ranking","targeted_mechanism":"none","kind":"harness","fixtures":["harness-goldens"]}
    assert total_gate(reference,score,manifest,nonranking,["scripts/telemetry/cost-gate.sh"]).exit_code==0
    broken=copy.deepcopy(score); broken["families"]["exact_recall"]["mrr_at_10"]=0.4; broken["fixture_results"]["harness-goldens"]=False
    result=total_gate(reference,broken,manifest,nonranking,["scripts/telemetry/cost-gate.sh"]); assert result.exit_code==1 and len(result.reasons)==2
    for family in ("exact_recall","concept_recall"):
        for metric in ("mrr_at_10","hit_at_1","hit_at_5"):
            candidate=copy.deepcopy(score); candidate["families"][family][metric]=max(0.0,reference["families"][family][metric]-0.1)
            assert total_gate(reference,candidate,manifest,nonranking,[]).exit_code==1
    candidate=copy.deepcopy(score); candidate["families"]["real_query"]["mrr_at_10"]=0.49; assert total_gate(reference,candidate,manifest,nonranking,[]).exit_code==1
    candidate=copy.deepcopy(score); candidate["shapes"]["identifier"]["mrr_at_10"]=0.489; assert total_gate(reference,candidate,manifest,nonranking,[]).exit_code==1
    candidate=copy.deepcopy(score); candidate["shapes"]["identifier"]["mrr_at_10"]=0.49; assert total_gate(reference,candidate,manifest,nonranking,[]).exit_code==0
    candidate=copy.deepcopy(score); candidate["shapes"]["identifier"]["hit_at_5"]=0.7; assert total_gate(reference,candidate,manifest,nonranking,[]).exit_code==1
    ranking={"slice_class":"ranking","targeted_mechanism":"topk_cut","kind":"ranking","fixtures":[]}
    mismatch=total_gate(reference,score,manifest,ranking,["scripts/telemetry/cost-gate.sh"]); assert mismatch.exit_code==2 and "descriptor_class_mismatch" in mismatch.reasons[0]
    missing=total_gate(reference,score,manifest,None,["crates/aft/src/query_shape.rs"]); assert missing.exit_code==1 and "missing ranking descriptor" in missing.reasons
    ranking_score=copy.deepcopy(score); ranking_score["mechanisms"]["topk_cut"]["mrr_at_10"]=0.6
    assert total_gate(reference,ranking_score,manifest,ranking,["crates/aft/src/query_shape.rs"]).exit_code==0
    duplicate=copy.deepcopy(score); duplicate["rows"].append(copy.deepcopy(duplicate["rows"][0])); duplicate["families"]["real_query"]["mrr_at_10"]=0.0
    combined=total_gate(reference,duplicate,manifest,nonranking,[]); assert combined.exit_code==2 and len(combined.reasons)==1
    invalid_request=copy.deepcopy(score); invalid_request["rows"][0]["request"]["offset"]=0; assert total_gate(reference,invalid_request,manifest,nonranking,[]).exit_code==2
    depth=copy.deepcopy(score); depth["rows"][0]["collapse_stop_reason"]="depth_cap"; assert total_gate(reference,depth,manifest,nonranking,[]).exit_code==2
    empty={"schema":"manifest","rows":[{"episode_id":"followup-census:1","excluded_reason":"repo_unowned_or_unavailable"}]}
    assert total_gate(reference,{"rows":[]},empty,nonranking,[]).exit_code==2
    pop={"window":{"start":"2026-09-09","end":"2026-12-08"},"seed":"x","strata":list(STRATA)}
    empty_est=estimator([{"census_stratum":name,"n_s":0,"f_s":0} for name in STRATA],pop,discriminating=0,all_episodes=1)
    assert empty_est=={"labelled_total":0,"population":pop,"estimator":"undefined_empty_population"}
    nonempty=estimator([{"census_stratum":name,"n_s":10,"f_s":index+1} for index,name in enumerate(STRATA)],pop,discriminating=40,all_episodes=100)
    assert len(nonempty["rows"])==5 and abs(sum(row["w_s"] for row in nonempty["rows"])-1)<0.00001
    assert nonempty["comparisons"]["conditional"]["prior_denominator"]==6469 and nonempty["comparisons"]["projected"]["prior_denominator"]==20183
    with tempfile.TemporaryDirectory() as directory:
        ref=Path(directory)/"reference.json"; side=Path(directory)/"manifest.sha256"; ref.write_bytes(b'{"old":1}\n'); side.write_bytes(b'{"old":2}\n'); old=(ref.read_bytes(),side.read_bytes())
        for fault in ("validation","write_reference","write_sidecar","fsync_reference","fsync_sidecar","rename_reference","between_renames","rename_sidecar"):
            try: atomic_write_pair(ref,side,b'{"new":1}\n',b'{"new":2}\n',fault=fault); raise AssertionError(fault)
            except InputFault: assert (ref.read_bytes(),side.read_bytes())==old
        atomic_write_pair(ref,side,b'{"new":1}\n',b'{"new":2}\n'); assert ref.read_bytes()==b'{"new":1}\n' and side.read_bytes()==b'{"new":2}\n'
    with tempfile.TemporaryDirectory() as directory:
        ref=Path(directory)/"reference.json"; side=Path(directory)/"manifest.sha256"
        try: atomic_write_pair(ref,side,b'{"new":1}\n',b'{"new":2}\n',fault="between_renames"); raise AssertionError("partial initial pair")
        except InputFault: assert not ref.exists() and not side.exists()
    old_manifest={"rows":[{"episode_id":"followup-census:1","query":"old"}]}; new_manifest={"rows":[{"episode_id":"followup-census:1","query":"new"},{"episode_id":"followup-census:2","query":"added"}]}
    assert identity_delta(old_manifest,new_manifest)=={"added":["followup-census:2"],"removed":[],"changed":["followup-census:1"]}
    manifest_path=BENCH/"real-query-manifest.json"
    production=read_json(manifest_path)
    assert production["census_unique_identities"]==6469 and production["retained_labels"]==300 and len(production["rows"])==300
    assert production["mechanism_projection_sum"]==6470


def old_json(revision:str,path:Path)->dict[str,Any]:
    relative=path.resolve().relative_to(ROOT)
    result=subprocess.run(["git","show",f"{revision}:{relative}"],cwd=ROOT,text=True,capture_output=True,check=False)
    if result.returncode: raise InputFault(f"manifest_maintenance_old_input_missing:{relative}")
    value=json.loads(result.stdout)
    if not isinstance(value,dict): raise InputFault("malformed_schema:old_input")
    return value


def run(args:argparse.Namespace)->int:
    if args.self_test:
        self_test(); print("search_quality_goldens:ok"); return 0
    manifest_path=Path(args.manifest); reference_path=Path(args.reference); sidecar_path=Path(args.sidecar)
    manifest=read_json(manifest_path)
    included_manifest_ids(manifest)
    if args.mode=="verify":
        if args.descriptor: raise InputFault("verify_rejects_descriptor")
        included_manifest_ids(manifest)
        if not args.score: raise InputFault("missing_score")
        verify_score=read_json(Path(args.score)); validate_scored_population(manifest,verify_score); validate_profile_score(verify_score)
        receipt=BENCH/".bench"/"verify-receipt.json"; receipt.parent.mkdir(parents=True,exist_ok=True); receipt.write_bytes(canonical_json({"head":subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip(),"manifest_sha256":sha256_file(manifest_path),"reference_sha256":sha256_file(reference_path) if reference_path.is_file() else None,"model_id":verify_score.get("model_id",args.model_id),"profile":verify_score.get("profile")}))
        print("verify:green"); return 0
    if args.mode=="record-reference":
        if args.descriptor: raise InputFault("record_reference_rejects_descriptor")
        included_manifest_ids(manifest)
        if not args.score: raise InputFault("missing_score")
        score=read_json(Path(args.score)); validate_scored_population(manifest,score); validate_profile_score(score)
        recorded=dict(score); recorded.update({"schema":"aft-search-reference-v1","evidence_sha":EVIDENCE_SHA,"manifest_sha256":sha256_file(manifest_path)})
        reference_bytes=canonical_json(recorded)
        old_reference: dict[str,Any] | None = None
        if args.manifest_changed:
            old_manifest=old_json(args.base_ref,manifest_path); old_reference=old_json(args.base_ref,reference_path)
            if not args.old_score: raise InputFault("manifest_maintenance_old_score_missing")
            old_score=read_json(Path(args.old_score))
            if old_score.get("model_id")!=score.get("model_id") or old_score.get("profile")!=score.get("profile"): raise InputFault("manifest_maintenance_binary_profile_mismatch")
            old_descriptor_path=descriptor_path(None,args.branch); descriptor=read_json(old_descriptor_path) if old_descriptor_path else None
            old_result=total_gate(old_reference,old_score,old_manifest,descriptor,diff_paths(args.base_ref,args.head))
            if old_result.exit_code: raise InputFault("old_manifest_evaluation:"+";".join(old_result.reasons))
            delta=identity_delta(old_manifest,manifest)
        else: delta={"added":[row["episode_id"] for row in manifest["rows"] if "excluded_reason" not in row],"removed":[],"changed":[]}
        atomic_write_pair(reference_path,sidecar_path,reference_bytes,sidecar_bytes(manifest_path,reference_bytes),fault=os.environ.get("AFT_REFERENCE_FAULT"))
        print(json.dumps({"old_reference_sha256":None if old_reference is None else sha256_bytes(canonical_json(old_reference)),"new_reference_sha256":sha256_bytes(reference_bytes),"identity_delta":delta},sort_keys=True)); return 0
    sidecar=binding(reference_path,manifest_path,sidecar_path)
    if not args.score: raise InputFault("missing_score")
    score=read_json(Path(args.score)); reference=read_json(reference_path)
    path=descriptor_path(args.descriptor,args.branch); descriptor=read_json(path) if path else None
    paths=diff_paths(args.base_ref,args.head)
    if args.rebaseline:
        if args.from_profile!="single_page" or args.to_profile!="paged" or descriptor is None: raise InputFault("rebaseline_precondition")
        receipt_path=BENCH/".bench"/"verify-receipt.json"; receipt=read_json(receipt_path)
        head=subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip()
        if receipt.get("head")!=head or receipt.get("manifest_sha256")!=sha256_file(manifest_path) or receipt.get("reference_sha256")!=sha256_file(reference_path): raise InputFault("rebaseline_receipt_mismatch")
        if reference.get("profile")!="single_page" or score.get("profile")!="paged" or score.get("capability",{}).get("offset_declared") is not True or score.get("capability",{}).get("probe_pages_differ") is not True: raise InputFault("rebaseline_profile_capability")
        if {row.get("episode_id") for row in reference.get("rows",[])}!={row.get("episode_id") for row in score.get("rows",[])}: raise InputFault("rebaseline_identity_mismatch")
    elif score.get("profile")!=reference.get("profile"):
        raise InputFault("illegal_profile:reference_profile_mismatch")
    if score.get("model_id")!=reference.get("model_id"): raise InputFault("corpus_vector_model_mismatch")
    result=total_gate(reference,score,manifest,descriptor,paths)
    for reason in result.reasons: print(reason,file=sys.stderr)
    if result.exit_code or not args.rebaseline: return result.exit_code
    recorded=dict(score); recorded.update({"schema":"aft-search-reference-v1","evidence_sha":EVIDENCE_SHA,"manifest_sha256":sha256_file(manifest_path)})
    reference_bytes=canonical_json(recorded); old_digest=sha256_file(reference_path)
    atomic_write_pair(reference_path,sidecar_path,reference_bytes,sidecar_bytes(manifest_path,reference_bytes),fault=os.environ.get("AFT_REFERENCE_FAULT"))
    print(json.dumps({"old_reference_sha256":old_digest,"new_reference_sha256":sha256_bytes(reference_bytes),"old_profile":"single_page","new_profile":"paged","capability":score["capability"]},sort_keys=True)); return 0


def parser()->argparse.ArgumentParser:
    p=argparse.ArgumentParser(); p.add_argument("--mode",choices=("evaluate","record-reference","verify"),default="evaluate"); p.add_argument("--manifest",default=str(BENCH/"real-query-manifest.json")); p.add_argument("--reference",default=str(BENCH/"real-query-baseline.json")); p.add_argument("--sidecar",default=str(BENCH/"manifest.sha256")); p.add_argument("--score"); p.add_argument("--descriptor"); p.add_argument("--branch"); p.add_argument("--base-ref",default="HEAD^"); p.add_argument("--head",default="HEAD"); p.add_argument("--model-id",default="aft-search-fixture-v1"); p.add_argument("--manifest-changed",action="store_true"); p.add_argument("--old-score"); p.add_argument("--rebaseline",action="store_true"); p.add_argument("--from-profile"); p.add_argument("--to-profile"); p.add_argument("--self-test",action="store_true"); return p

def main()->int:
    try: return run(parser().parse_args())
    except (InputFault,OSError,ValueError,json.JSONDecodeError) as error: print(str(error),file=sys.stderr); return 2
if __name__=="__main__": raise SystemExit(main())
