#!/usr/bin/env bash
# Exercise the production stored grammar and Luau outline under the signed
# binary, offline and without test seams. Used by daemon staging and npm CI.
# Usage: ck_aft_smoke_grammar <binary> <error-prefix>
ck_aft_smoke_grammar() (
  local binary="$1" prefix="$2" repo work hash
  repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)" || return 1
  mkdir -p "$repo/target/scratch" || return 1
  work="$(mktemp -d "$repo/target/scratch/grammar-smoke.XXXXXX")" || return 1
  trap 'rm -rf "$work"' EXIT
  hash="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["sha256"])' "$repo/crates/aft-parse/src/grammar/manifest.json")" || return 1
  mkdir -p "$work/storage/grammars" "$work/home" "$work/config" "$work/cache" "$work/state" "$work/data" "$work/tmp" || return 1
  cp "$repo/crates/aft/tests/fixtures/grammars/$hash.wasm" "$work/storage/grammars/$hash.wasm" || return 1

  echo "==> smoke: pinned grammar compile under the signed binary (offline)"
  if ! sandbox-exec -p '(version 1) (allow default) (deny network*)' env \
      -u AFT_STORAGE_DIR -u AFT_CACHE_DIR -u AFT_TEST_GRAMMAR_MANIFEST \
      -u AFT_TEST_GRAMMAR_STORAGE -u AFT_TEST_GRAMMAR_QUERIES -u AFT_TEST_GRAMMAR_FAULT \
      -u AFT_TEST_GRAMMAR_TRANSPORT_FILE -u AFT_TEST_GRAMMAR_CLOCK_FILE \
      -u AFT_TEST_GRAMMAR_EXTRACTOR_VERSION \
      HOME="$work/home" XDG_CONFIG_HOME="$work/config" \
      XDG_CACHE_HOME="$work/cache" XDG_STATE_HOME="$work/state" \
      XDG_DATA_HOME="$work/data" TMPDIR="$work/tmp" \
      "$binary" grammar-smoke --storage-dir "$work/storage" > "$work/result.json"; then
    echo "$prefix: signed grammar load failed" >&2
    return 1
  fi
  if ! python3 -c 'import json,sys; v=json.load(open(sys.argv[1])); assert not v["test_seams"]; assert v["engine_target"] == "pulley"; assert v["hash"] == sys.argv[2]; assert v["tree"].startswith("(chunk ")' "$work/result.json" "$hash"; then
    echo "$prefix: grammar smoke did not verify the production grammar on Pulley" >&2
    return 1
  fi
  echo "    smoke: production grammar $hash loaded on Pulley without test seams"
  echo "==> smoke: Luau outline under the signed binary (offline)"
  mkdir -p "$work/project" || return 1
  cp "$repo/crates/aft/tests/fixtures/grammars/smoke.luau" "$work/project/smoke.luau" || return 1
  if ! env -u AFT_STORAGE_DIR -u AFT_CACHE_DIR -u AFT_TEST_GRAMMAR_MANIFEST \
      -u AFT_TEST_GRAMMAR_STORAGE -u AFT_TEST_GRAMMAR_QUERIES -u AFT_TEST_GRAMMAR_FAULT \
      -u AFT_TEST_GRAMMAR_TRANSPORT_FILE -u AFT_TEST_GRAMMAR_CLOCK_FILE \
      -u AFT_TEST_GRAMMAR_EXTRACTOR_VERSION -u AFT_TEST_GRAMMAR_PARSE_DEADLINE_MS \
      HOME="$work/home" XDG_CONFIG_HOME="$work/config" \
      XDG_CACHE_HOME="$work/cache" XDG_STATE_HOME="$work/state" \
      XDG_DATA_HOME="$work/data" TMPDIR="$work/tmp" \
      python3 - "$binary" "$work" <<'PY'
import json, pathlib, signal, subprocess, sys, time
binary, root = str(pathlib.Path(sys.argv[1]).resolve()), pathlib.Path(sys.argv[2]).resolve()
def timed_out(*_):
    raise TimeoutError("signed outline smoke timed out")
signal.signal(signal.SIGALRM, timed_out)
signal.alarm(45)
with open(root / "outline-stderr.log", "w") as stderr:
    process = subprocess.Popen(["sandbox-exec", "-p", "(version 1) (allow default) (deny network*)", binary],
                               cwd=root / "project", stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True)
    try:
        def request(value, retry_unavailable=False):
            while True:
                process.stdin.write(json.dumps(value) + "\n")
                process.stdin.flush()
                reply = None
                for line in process.stdout:
                    candidate = json.loads(line)
                    if candidate.get("id") == value["id"]:
                        reply = candidate
                        break
                if reply is None:
                    raise RuntimeError("signed binary exited before replying")
                # The first Luau request starts loading the stored grammar in the
                # background and answers grammar_unavailable ("retry in a few
                # seconds") until it is ready; the 45 s alarm bounds the wait.
                if retry_unavailable and reply.get("code") == "grammar_unavailable":
                    time.sleep(0.5)
                    continue
                assert reply.get("success"), reply
                return reply
        request({"id": "signed-configure", "command": "configure", "harness": "runner",
                 "project_root": str(root / "project"), "storage_dir": str(root / "storage"),
                 "config": [{"tier": "user", "source": str(root / "config" / "aft.jsonc"),
                             "doc": json.dumps({"storage": {"cache_dir": None}, "indexes": {"semantic": False}})}]})
        reply = request({"id": "signed-outline", "command": "outline", "file": str(root / "project" / "smoke.luau")},
                        retry_unavailable=True)
        assert "signed_outline_fixture" in reply.get("text", ""), reply
        (root / "outline-result.json").write_text(json.dumps(reply))
    finally:
        process.kill()
        process.wait()
signal.alarm(0)
PY
  then
    cat "$work/outline-stderr.log" >&2
    echo "$prefix: signed Luau outline failed" >&2
    return 1
  fi
  echo "    smoke: Luau outline contains signed_outline_fixture"
)
