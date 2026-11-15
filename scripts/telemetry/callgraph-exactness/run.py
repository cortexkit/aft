#!/usr/bin/env python3
"""Fail-closed stored/query/refresh comparison of two test-hooks probe binaries."""
from __future__ import annotations

import argparse
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import random
import re
import shutil
import subprocess
import sys
import textwrap
import time

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
spec = importlib.util.spec_from_file_location("exactness_matrix", HERE.parent / "index-matrix/run.py")
MATRIX = importlib.util.module_from_spec(spec)
spec.loader.exec_module(MATRIX)


def save(path, value):
    MATRIX.save(path, value)


def compare(left, right):
    """Compare bytes, never a hash of an artifact against itself."""
    a = json.loads((left / "files.json").read_text())
    b = json.loads((right / "files.json").read_text())
    a = {row["path"]: (row["record"], row["fragment"]) for row in a}
    b = {row["path"]: (row["record"], row["fragment"]) for row in b}
    for path in sorted(a.keys() | b.keys()):
        if a.get(path) != b.get(path):
            raise ValueError(f"{path}: published fragment-key/record column differs: {a.get(path)} != {b.get(path)}")
    names = {path.relative_to(left) for path in (left / "sections").iterdir()}
    other = {path.relative_to(right) for path in (right / "sections").iterdir()}
    if names != other:
        raise ValueError(f"section set differs: {names ^ other}")
    for name in sorted(names | {Path("sections.json"), Path("resolutions.bin"), Path("queries.jsonl"), Path("dead-code.json"), Path("dead-code-findings.json")}):
        with (left / name).open("rb") as a, (right / name).open("rb") as b:
            position = 0
            while True:
                x, y = a.read(1 << 20), b.read(1 << 20)
                if x != y:
                    raise ValueError(f"{name}: bytes differ near offset {position}; artifacts: {left}, {right}")
                if not x:
                    break
                position += len(x)
    if (left / "raw").exists() and (right / "raw").exists():
        raw_a, raw_b = left / "raw/sections", right / "raw/sections"
        if {p.name for p in raw_a.iterdir()} != {p.name for p in raw_b.iterdir()}:
            raise ValueError("raw cold section set differs")
        for path in raw_a.iterdir():
            with path.open("rb") as a, (raw_b / path.name).open("rb") as b:
                while True:
                    x, y = a.read(1 << 20), b.read(1 << 20)
                    if x != y:
                        raise ValueError(f"raw cold section {path.name}: bytes differ")
                    if not x:
                        break
    for folder in (left, right):
        health = json.loads((folder / "health.json").read_text())
        if health["stored_slices_failures"] or health["full_resolutions"] <= 0:
            raise ValueError(f"empty or damaged publication: {folder}: {health}")


INITIAL = {
    "lib.ts": "export function sink(value: number) { return value + 1; }\nexport function orphan(value: number) { return value - 1; }\n",
    "alternative.ts": "export function alternate(value: number) { return value * 2; }\n",
    "service.ts": "export class Base { run(value: number) { return value; } }\nexport class Other { run(value: number) { return value + 2; } }\nexport class Worker extends Base {}\nexport function method(value: number) { const worker: Base = new Worker(); return worker.run(value); }\n",
    "main.ts": "import { sink } from './lib';\nimport { method } from './service';\nexport function entry(value: number) {\n  const forwarded = sink(value);\n  return method(forwarded);\n}\n",
    "remove.ts": "export function removed() { return 1; }\n",
    "rename_me.ts": "export function renamed() { return 1; }\n",
    "tsconfig.json": '{"compilerOptions":{"baseUrl":".","paths":{"@gate/*":["./*"]}}}\n',
    "go.mod": "module example.com/exactness\n\ngo 1.23\n",
    "iface.go": "package exactness\ntype Required interface { Run() }\ntype Thing struct {}\nfunc (Thing) Run() {}\nfunc invoke(value Required) { value.Run() }\n",
}


def island(root, round_):
    folder = root / "exactness_gate"
    folder.mkdir(exist_ok=True)
    if round_ == 0:
        if any(folder.iterdir()):
            raise ValueError("witness island already exists")
        for name, source in INITIAL.items():
            (folder / name).write_text(source)
        return []
    changes = []
    def put(name, source):
        (folder / name).write_text(source)
        changes.append("exactness_gate/" + name)
    if round_ == 1:
        put("added.ts", "export function added(value: number) { return value; }\n")
        (folder / "remove.ts").unlink()
        changes.append("exactness_gate/remove.ts")
        (folder / "rename_me.ts").rename(folder / "renamed.ts")
        changes.extend(["exactness_gate/rename_me.ts", "exactness_gate/renamed.ts"])
        put("service.ts", INITIAL["service.ts"].replace("extends Base", "extends Other").replace("worker: Base", "worker: Other").replace("return value + 2", "return value + 7"))
        put("main.ts", INITIAL["main.ts"].replace("'./service'", "'@gate/service'"))
        put("tsconfig.json", '{"compilerOptions":{"baseUrl":".","paths":{"@gate/*":["./*","../*"]}}}\n')
        put("iface.go", INITIAL["iface.go"].replace("Run() }", "Run(); Extra() }"))
    else:
        put("iface.go", (folder / "iface.go").read_text() + "func (Thing) Extra() {}\n")
        put("service.ts", (folder / "service.ts").read_text().replace("extends Other", "extends Base").replace("worker: Other", "worker: Base"))
        put("added.ts", "export function added(value: number) { return value + 9; }\n")
    return changes


def native_edits(binary, root, plan, seed):
    """Derive pin-independent edits from the reference's own stored graph."""
    rng = random.Random(seed)
    files = sorted(plan["files"])
    rng.shuffle(files)
    if len(files) < 2:
        raise ValueError("need two native files for delete/rename witnesses")
    deleted, renamed = files[:2]
    originals = {name: (root / name).read_bytes() for name in (deleted, renamed)}
    edits = []
    rejected = []

    def valid(row, source):
        path = root / row["file"]
        before = path.read_bytes()
        path.write_text(source)
        result = subprocess.run([str(binary), "validate", str(path)], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        path.write_bytes(before)
        if result.returncode:
            rejected.append({"file": row["file"], "symbol": row["symbol"]["scoped_name"], "reason": result.stderr.decode()[-1000:]})
        return result.returncode == 0

    member_edit = None
    for member in plan["members"]:
        if member["sites"] == 0:
            continue
        for row in member["declarations"]:
            if row["file"] in originals:
                continue
            path = root / row["file"]
            source = path.read_text()
            lines = source.splitlines(keepends=True)
            symbol = row["symbol"]
            # BlobSymbol spans are zero-based with an inclusive end row.
            start, end = symbol["start_line"], symbol["end_line"] + 1
            declaration = "".join(lines[start:end])
            language = row["language"]
            if language in ("typescript", "tsx", "javascript"):
                added = "\nclass ExactnessGateNative {\n" + declaration + "\n}\n"
            elif language == "python":
                added = "\nclass ExactnessGateNative:\n" + textwrap.indent(textwrap.dedent(declaration), "    ") + "\n"
            elif language == "go":
                added = "\ntype ExactnessGateNative struct {}\n" + re.sub(r"\b" + re.escape(row["owner"]) + r"\b", "ExactnessGateNative", declaration, count=1)
            elif language == "lua":
                added = "\nExactnessGateNative = {}\n" + declaration.replace(row["owner"], "ExactnessGateNative", 1)
            else:
                rejected.append({"file": row["file"], "reason": f"no syntax-preserving new-owner template for {language}"})
                continue
            duplicate = source + added
            removed = "".join(lines[:start] + lines[end:])
            if not declaration.strip() or not valid(row, duplicate) or not valid(row, removed):
                continue
            member_edit = {"file": row["file"], "member": member["member"], "sites": member["sites"], "round1": duplicate, "round2": removed, "declaration": symbol}
            originals[row["file"]] = path.read_bytes()
            break
        if member_edit:
            break
    if not member_edit:
        raise ValueError(f"no parseable native member duplication/removal: {rejected}")
    signature_edit = None
    for row in plan["top_callees"]:
        if row["file"] in originals:
            continue
        path = root / row["file"]
        source = path.read_text()
        lines = source.splitlines(keepends=True)
        line = row["symbol"]["start_line"]
        signature = lines[line]
        # Rename a parameter, not the callee: body references may become
        # unresolved, which is intentional and must refresh identically.
        pattern = r"\((?:self\s*,\s*)?([A-Za-z_][A-Za-z_0-9]*)\s*(?=[:,)])"
        if row["language"] == "go":
            # Skip the receiver when this is a method.
            offset = signature.find(")") + 1 if re.match(r"\s*func\s*\(", signature) else 0
            match = re.search(r"\(\s*([A-Za-z_][A-Za-z_0-9]*)\s+[A-Za-z_*\[\]]", signature[offset:])
            if match:
                begin, finish = offset + match.start(1), offset + match.end(1)
        elif row["language"] in ("c", "cpp"):
            match = re.search(r"\((?:const\s+)?[A-Za-z_][A-Za-z_0-9]*(?:\s+|\s*\*\s*)([A-Za-z_][A-Za-z_0-9]*)\s*(?=[,)\[])", signature)
            if match:
                begin, finish = match.start(1), match.end(1)
        else:
            match = re.search(pattern, signature)
            if match:
                begin, finish = match.start(1), match.end(1)
        if not match:
            continue
        replacement = signature[:begin] + match[1] + "_exactness" + signature[finish:]
        lines[line] = replacement
        changed = "".join(lines)
        if valid(row, changed):
            signature_edit = {"file": row["file"], "symbol": row["symbol"], "callers": row["callers"], "round1": changed}
            originals[row["file"]] = path.read_bytes()
            break
    if not signature_edit:
        raise ValueError("no parseable signature edit among reference top ten callees")
    edits.extend([member_edit, signature_edit])
    return {"delete": deleted, "rename": renamed, "renamed_to": renamed + ".exactness" + Path(renamed).suffix, "edits": edits, "rejected": rejected}, originals


def apply_native(root, native, round_):
    changes = []
    if round_ == 1:
        (root / native["delete"]).unlink()
        (root / native["rename"]).rename(root / native["renamed_to"])
        changes.extend([native["delete"], native["rename"], native["renamed_to"]])
    for edit in native["edits"]:
        if f"round{round_}" in edit:
            (root / edit["file"]).write_text(edit[f"round{round_}"])
            changes.append(edit["file"])
    return changes


def probe(args, binary, mode, root, store, out, threads, reference=None, changes=None, flip=False):
    out.mkdir(parents=True, exist_ok=False)
    command = [str(binary), mode, str(root), str(store), str(out), str(threads),
               str(args.cache_bytes), str(args.sample), str(reference) if reference else "-"]
    if changes:
        command.append(str(changes))
    env = {k: v for k, v in os.environ.items() if not k.startswith("AFT_")}
    env.update(AFT_CALLGRAPH_BUILD_CACHE_BYTES=str(args.build_cache_bytes),
               AFT_CALLGRAPH_SPILL_BYTES=str(args.spill_bytes))
    if flip:
        env["AFT_EXACTNESS_FLIP_SITE"] = "exactness_gate/service.ts"
    started = time.monotonic()
    with (out / "probe.log").open("w") as log:
        process = subprocess.Popen(command, env=env, stdout=log, stderr=subprocess.STDOUT)
        while True:
            remaining = args.timeout_s - (time.monotonic() - started)
            if remaining <= 0:
                process.kill()
                process.wait()
                raise TimeoutError(f"{out}: {mode} exceeded {args.timeout_s}s")
            try:
                code = process.wait(timeout=min(60, remaining))
                break
            except subprocess.TimeoutExpired:
                print(f"exactness {out.name}: {mode} still running ({time.monotonic()-started:.0f}s)", flush=True)
    elapsed = time.monotonic() - started
    row = {"mode": mode, "threads": threads, "seconds": elapsed, "exit_code": code, "command": command}
    if code == 0:
        operations = {}
        positive_paths = 0
        errors = 0
        for line in (out / "queries.jsonl").read_text().splitlines():
            query = json.loads(line)
            op = query["query"]["op"]
            operations[op] = operations.get(op, 0) + 1
            positive_paths += bool(query["query"]["positive"])
            errors += "error" in query["answer"]
        row.update(queries=operations, positive_paths=positive_paths, query_errors=errors,
                   health=json.loads((out / "health.json").read_text()),
                   slice_cache=json.loads((out / "cache.json").read_text()))
    save(out / "run.json", row)
    print(f"exactness {out}: {elapsed:.2f}s exit={code}", flush=True)
    if code:
        raise ValueError(f"{out}: probe failed: {(out / 'probe.log').read_text()[-6000:]}")
    return row


def run_repository(args, entry):
    folder = args.results / entry["repository"].replace("/", "--")
    folder.mkdir(parents=True, exist_ok=True)
    if (folder / "result.json").exists():
        raise ValueError(f"choose a new results folder: {folder}")
    root = MATRIX.checkout(entry, folder).resolve()
    result = {"repository": entry["repository"], "pin": entry["commit"], "completed": False,
              "reference_sha256": MATRIX.sha256(args.reference), "candidate_sha256": MATRIX.sha256(args.candidate),
              "reference_source": args.reference_source, "candidate_source": args.candidate_source,
              "threads": args.threads, "cache_bytes": args.cache_bytes,
              "build_cache_bytes": args.build_cache_bytes, "spill_bytes": args.spill_bytes,
              "future_knobs": "AFT_CALLGRAPH_BUILD_CACHE_BYTES and AFT_CALLGRAPH_SPILL_BYTES passed through; legacy builder ignores them",
              "section_comparison": "byte equality by section ID after copy-only fold on both sides; physical order excluded",
              "runs": []}
    started = time.monotonic()
    originals = {}
    native = None
    try:
        island(root, 0)
        reference = folder / "reference-0"
        result["runs"].append(probe(args, args.reference, "cold", root, folder / "reference-store-0", reference, 1))
        plan = json.loads((reference / "native.json").read_text())
        native, originals = native_edits(args.reference, root, plan, args.seed)
        save(folder / "native-edits.json", native)
        candidates = {}
        for threads in args.threads:
            out = folder / f"candidate-{threads}-0"
            store = folder / f"candidate-store-{threads}"
            result["runs"].append(probe(args, args.candidate, "cold", root, store, out, threads, reference))
            compare(reference, out)
            candidates[threads] = store
        if args.negative_control:
            out = folder / "broken-candidate"
            try:
                probe(args, args.candidate, "cold", root, folder / "broken-store", out, 1, reference, flip=True)
                compare(reference, out)
            except ValueError as error:
                message = str(error)
                if "exactness_gate/service.ts" not in message or "CONTROL flipped" not in (out / "probe.log").read_text():
                    raise ValueError(f"negative control failed for the wrong reason: {message}") from error
                result["negative_control"] = {"detected": True, "file": "exactness_gate/service.ts", "error": message}
            else:
                raise ValueError("deliberately broken candidate passed")
        for round_ in (1, 2):
            previous = reference
            changed = island(root, round_) + apply_native(root, native, round_)
            changes = folder / f"changes-{round_}.json"
            save(changes, changed)
            reference = folder / f"reference-{round_}"
            result["runs"].append(probe(args, args.reference, "cold", root, folder / f"reference-store-{round_}", reference, 1))
            before = {row["path"]: row["fragment"] for row in json.loads((previous / "files.json").read_text())}
            after = {row["path"]: row["fragment"] for row in json.loads((reference / "files.json").read_text())}
            affected = [path for path in sorted(before.keys() | after.keys()) if path not in changed and before.get(path) != after.get(path)]
            result.setdefault("refresh_fanout", []).append({"round": round_, "unedited_changed_fragments": len(affected), "examples": affected[:20]})
            for threads, store in candidates.items():
                # Both invocations exec a new process. The refresh reads only
                # the previous publication and the explicit changed paths.
                out = folder / f"refresh-{threads}-{round_}"
                result["runs"].append(probe(args, args.candidate, "refresh", root, store, out, threads, reference, changes))
                compare(reference, out)
                cold = folder / f"candidate-cold-{threads}-{round_}"
                result["runs"].append(probe(args, args.candidate, "cold", root, folder / f"candidate-cold-store-{threads}-{round_}", cold, threads, reference))
                compare(out, cold)
                compare(reference, cold)
        result["completed"] = True
    except Exception as error:
        result["error"] = str(error)
        raise
    finally:
        for name, content in originals.items():
            (root / name).write_bytes(content)
        if native:
            (root / native["renamed_to"]).unlink(missing_ok=True)
        shutil.rmtree(root / "exactness_gate", ignore_errors=True)
        result["seconds"] = time.monotonic() - started
        save(folder / "result.json", result)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--reference-source", required=True)
    parser.add_argument("--candidate-source", required=True)
    parser.add_argument("--manifest", type=Path, default=HERE.parent / "index-matrix/manifest.json")
    parser.add_argument("--repository", action="append", required=True)
    parser.add_argument("--results", type=Path, required=True)
    parser.add_argument("--threads", type=int, nargs="+", default=[1, 16])
    parser.add_argument("--cache-bytes", type=int, default=256 << 20, help="DataLayer block cache")
    parser.add_argument("--build-cache-bytes", type=int, default=4096, help="reserved builder knob, passed as AFT_CALLGRAPH_BUILD_CACHE_BYTES")
    parser.add_argument("--spill-bytes", type=int, default=4096, help="reserved builder knob, passed as AFT_CALLGRAPH_SPILL_BYTES")
    parser.add_argument("--sample", type=int, default=3, help="symbols per stratum, plus positive/negative witnesses")
    parser.add_argument("--seed", type=int, default=1441)
    parser.add_argument("--timeout-s", type=int, default=14400, help="per probe, not whole project")
    parser.add_argument("--negative-control", action="store_true")
    args = parser.parse_args()
    for key in ("reference", "candidate", "manifest", "results"):
        setattr(args, key, getattr(args, key).resolve())
    if args.sample < 1 or min(args.threads) < 1 or args.cache_bytes < 1 or args.timeout_s < 1:
        parser.error("sample, threads, cache and timeout must be positive")
    args.results.mkdir(parents=True, exist_ok=True)
    entries = {row["repository"]: row for row in json.loads(args.manifest.read_text())["repositories"]}
    # Lock the whole kit, not one results folder: independent invocations
    # must not overlap large runs even if they choose different output paths.
    lock_path = REPO / "target/measurements/callgraph-exactness.lock"
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        for repository in args.repository:
            run_repository(args, entries[repository])
    print("EXACTNESS GATES PASSED", flush=True)


if __name__ == "__main__":
    main()
