#!/usr/bin/env python3
"""Read-only storage census. Never starts AFT, repairs SQLite, or applies a plan.

SQLite is queried ONLY by a separate sqlite3 -readonly process. A checkpointed
file without a WAL uses immutable=1 to prevent SQLite creating missing sidecars;
immutable is never used when a WAL exists. Failed probes are explicit gaps, not
permission to delete. File-set sizes come from lstat, never reading WAL/SHM bytes.
"""
import argparse
import collections
import datetime
import hashlib
import json
import os
import pathlib
import re
import socket
import subprocess
import time
import urllib.parse

KEY = re.compile(r"^[0-9a-f]{16}$")
DOMAINS = {"views", "callgraph", "inspect", "index", "semantic", "symbols"}
AGE_MS = 7 * 86400 * 1000


def scope(path):
    # Existing roots use realpath; gone roots were recorded canonical at bind.
    normalized = os.path.realpath(path) if os.path.exists(path) else os.path.normpath(path)
    return hashlib.sha256(normalized.encode()).hexdigest()[:16]


def read_json(path):
    with open(path, encoding="utf-8") as stream:
        return json.load(stream)


def sqlite_ro(path, sql):
    wal = pathlib.Path(str(path) + "-wal")
    uri = "file:" + urllib.parse.quote(str(path), safe="/")
    # No immutable shortcut is permitted over uncheckpointed data.
    if not wal.exists() or wal.stat().st_size == 0:
        uri += "?immutable=1"
    result = subprocess.run(["sqlite3", "-readonly", "-json", uri, sql],
                            capture_output=True, text=True, timeout=120)
    if result.returncode:
        raise RuntimeError(result.stderr.strip())
    return json.loads(result.stdout or "[]")


def process_live(metadata):
    if metadata.get("hostname", socket.gethostname()) != socket.gethostname():
        return True  # a read-only plan does not expire another host's protection
    pid = metadata.get("pid", metadata.get("owner", {}).get("pid"))
    if not pid:
        return True
    try:
        os.kill(pid, 0)
        return True  # PID reuse uncertainty retains, rather than authorizing deletion
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def protections(directory):
    held = set()
    for path in directory.glob("pins/*.json"):
        try:
            metadata = read_json(path)
            if process_live(metadata):
                held.add(metadata["generation"])
        except (OSError, ValueError, KeyError):
            held.add("*")
    for path in directory.glob("readers/*/*.json"):
        try:
            if process_live(read_json(path)):
                held.add(path.parent.name)
        except (OSError, ValueError):
            held.add("*")
    return held


def manifest_keys(value):
    if isinstance(value, dict):
        for key, child in value.items():
            if key in {"semantic", "callgraph", "trigram", "segment", "key"} and isinstance(child, str):
                if re.fullmatch(r"[0-9a-f]{64}", child):
                    yield child.upper()
            yield from manifest_keys(child)
    elif isinstance(value, list):
        for child in value:
            yield from manifest_keys(child)


def files(root):
    """No symlink descent, no database content reads, count allocated blocks."""
    for directory, dirs, names in os.walk(root, followlinks=False):
        dirs[:] = [name for name in dirs if not pathlib.Path(directory, name).is_symlink()]
        for name in names:
            path = pathlib.Path(directory, name)
            try:
                stat = path.lstat()
            except FileNotFoundError:
                continue
            yield path, stat


def census(root):
    now = int(time.time() * 1000)
    identities = collections.defaultdict(list)
    gaps = []
    memo = read_json(root / "cache-keys.json") if (root / "cache-keys.json").exists() else {}
    for path, entry in memo.items():
        record = {"root": path, "last_bound_ms": entry.get("recorded_at_ms", now)}
        identities[scope(path)].append(record)
        identities[entry["key"]].append(record)
    for owner_path in root.glob("artifact-owners/*/owner.json"):
        try:
            owner = read_json(owner_path)
            record = {"root": owner["checkout_path"], "last_bound_ms": owner["heartbeat_at_ms"],
                      "protected": process_live(owner)}
            identities[owner_path.parent.name].append(record)
            identities[scope(record["root"])].append(record)
        except (OSError, ValueError, KeyError) as error:
            gaps.append(f"{owner_path}: {error}")
    for record_path in root.glob("retention/roots/*.json"):
        try:
            record = read_json(record_path)
            identities[record_path.stem].append(record)
            identities[record["artifact_key"]].append(record)
        except (OSError, ValueError, KeyError) as error:
            gaps.append(f"{record_path}: {error}")
    for registry in root.glob("blobs/v2/*/members.sqlite"):
        try:
            for member in sqlite_ro(registry, "SELECT scope, hex(root) AS root_hex, last_bind_ms FROM members"):
                if member["root_hex"]:
                    path = bytes.fromhex(member["root_hex"]).decode("utf-8")
                    record = {"root": path, "last_bound_ms": member["last_bind_ms"]}
                    identities[member["scope"]].append(record)
                    identities[registry.parent.name].append(record)
        except (RuntimeError, ValueError, subprocess.TimeoutExpired) as error:
            gaps.append(f"{registry}: {error}")

    inventory = list(files(root))
    sizes = collections.Counter()
    directories = {}
    referenced = set()
    plans = collections.defaultdict(int)
    directory_count = collections.Counter()
    for path, stat in inventory:
        parts = path.relative_to(root).parts
        if len(parts) >= 4 and parts[:2] == ("views", "v2") and KEY.fullmatch(parts[2]):
            domain, key = "views/v2", parts[2]
            base = root / "views" / "v2" / key
        elif len(parts) >= 3 and parts[0] in DOMAINS and KEY.fullmatch(parts[1]):
            domain, key = parts[:2]
            base = root / domain / key
        elif len(parts) >= 3 and parts[1] in DOMAINS and parts[0] not in DOMAINS:
            domain = parts[0] + "/" + parts[1]
            key = parts[2][:16]
            if not KEY.fullmatch(key):
                sizes[(domain, "shared/operational")] += stat.st_blocks * 512
                continue
            base = root / parts[0] / parts[1]
        else:
            sizes[(parts[0], "shared/operational")] += stat.st_blocks * 512
            continue

        identity = (str(base), key)
        if identity not in directories:
            records = identities.get(key, [])
            live = any(os.path.exists(item["root"]) for item in records)
            held = protections(base)
            protected = bool(held) or any(item.get("protected") for item in records)
            current = set()
            if base.joinpath("pointer.sqlite").exists():
                try:
                    current.update(row["generation"] for row in sqlite_ro(base / "pointer.sqlite",
                        "SELECT generation FROM pointer WHERE singleton=1"))
                except (RuntimeError, subprocess.TimeoutExpired) as error:
                    held.add("*")
                    gaps.append(f"{base}/pointer.sqlite: {error}")
            for pointer in base.glob(key + ".current"):
                current.add(pointer.read_text().strip())
            if "*" in held:
                current.add("*")
            else:
                current.update(held)
            for generation in list(current):
                reference = base / f"derived-{generation}.ref"
                if reference.is_file():
                    current.add(reference.read_text().strip())
            # Keys are content addressed globally. Mark every retained live manifest,
            # including pinned generations, not only this project's current one.
            for manifest in base.glob("manifest-*.json"):
                generation = manifest.name[9:-5]
                if live or protected:
                    if generation in current or "*" in current:
                        try:
                            referenced.update(manifest_keys(read_json(manifest)))
                        except (ValueError, OSError) as error:
                            gaps.append(f"{manifest}: {error}")
            old = bool(records) and all(now - item["last_bound_ms"] >= AGE_MS and
                not item["root"].startswith("/Volumes/") for item in records)
            directories[identity] = (live, protected, current, old, records)
            directory_count[domain] += 1
        live, protected, current, old, records = directories[identity]
        generation = None
        match = re.match(r"(?:derived-|manifest-|trigram-)(.+)\.(?:sqlite|json|bin|ref)(?:-(?:wal|shm))?$", path.name)
        if match:
            generation = match.group(1)
        elif re.match(re.escape(key) + r"\.g.*\.sqlite(?:-(?:wal|shm))?$", path.name):
            generation = re.sub(r"-(?:wal|shm)$", "", path.name)
        category = "live/current+coordination" if live else "dead/unknown root"
        if live and generation and generation not in current and "*" not in current:
            category = "live/superseded"
            plans[(str(base), generation, "unheld superseded generation")] += stat.st_blocks * 512
        elif not live and old and not protected:
            plans[(str(base), key, "missing root and last bound >=7 days")] += stat.st_blocks * 512
        sizes[(domain, category)] += stat.st_blocks * 512

    blobs = []
    for database in list(root.glob("blobs/*/*.sqlite")) + list(root.glob("blobs/v2/*/*.sqlite")):
        if database.name not in {"semantic.sqlite", "callgraph.sqlite", "trigram.sqlite"}:
            continue
        try:
            rows = sqlite_ro(database, "SELECT hex(full_key) AS key, length(payload) AS bytes FROM blob_payloads")
            used = sum(row["bytes"] for row in rows if row["key"] in referenced)
            total = sum(row["bytes"] for row in rows)
            free = sqlite_ro(database, "SELECT (SELECT freelist_count FROM pragma_freelist_count) * "
                             "(SELECT page_size FROM pragma_page_size) AS bytes")[0]["bytes"]
            blobs.append({"path": str(database.relative_to(root)), "rows": len(rows),
                          "payload_bytes": total, "live_referenced_payload_bytes": used,
                          "unreferenced_payload_bytes": total - used, "free_page_bytes": free})
        except (RuntimeError, subprocess.TimeoutExpired) as error:
            gaps.append(f"{database}: {error}")
    return {"at_utc": datetime.datetime.utcnow().isoformat() + "Z", "storage_root": str(root),
            "mode": "read-only advisory dry run; no apply mode", "missing_root_grace_days": 7,
            "allocated_bytes": sum(sizes.values()), "directory_counts": dict(directory_count),
            "table": [{"domain": domain, "class": category, "allocated_bytes": size}
                      for (domain, category), size in sorted(sizes.items())], "blobs": blobs,
            "would_delete": [{"directory": base, "generation_or_key": key, "reason": reason,
                              "allocated_bytes": size} for (base, key, reason), size in sorted(plans.items())],
            "gaps": gaps,
            "caveats": ["Allocated blocks, not APFS exclusive extents; concurrent live writes can change totals.",
                        "Missing/unrecognised keys are dead-weight classification, NOT immediate deletion authority.",
                        "Superseded candidates still require the writer/pointer lock and liveness recheck at apply time.",
                        "Blob numbers are payload lengths; they are not equivalent to physical reclaimable file bytes."]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--storage-root", type=pathlib.Path, required=True)
    args = parser.parse_args()
    print(json.dumps(census(args.storage_root.resolve()), indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
