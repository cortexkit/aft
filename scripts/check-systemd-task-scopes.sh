#!/usr/bin/env bash
set -euo pipefail

cargo --version
for selection in aft-engine:20 aft-tools:1; do
  package=${selection%:*}
  expected=${selection#*:}
  inventory="$RUNNER_TEMP/task-scope-$package-inventory.txt"
  output="$RUNNER_TEMP/task-scope-$package-tests.txt"
  args=(--features test-hooks -p "$package" --lib -- task_scope --ignored)
  cargo test "${args[@]}" --list > "$inventory"
  python3 - "$package" "$expected" "$inventory" <<'PY'
import pathlib, sys
package, expected, path = sys.argv[1:]
tests = [line for line in pathlib.Path(path).read_text().splitlines() if line.endswith(': test')]
if len(tests) != int(expected):
    raise SystemExit(f'{package}: expected {expected} systemd user-manager tests, found {len(tests)}: {tests}')
print(f'{package}: found {len(tests)} systemd user-manager tests')
PY
  # These tests assert manager availability; --ignored opts them in.
  if ! cargo test "${args[@]}" --test-threads=1 --nocapture > "$output" 2>&1; then
    cat "$output"
    exit 1
  fi
  cat "$output"
  python3 - "$package" "$expected" "$output" <<'PY'
import pathlib, re, sys
package, expected, path = sys.argv[1:]
output = pathlib.Path(path).read_text()
# Cargo ran one library harness; a self-exec child can print an earlier
# summary, but only the final line counts the selected suite.
summaries = re.findall(r'^test result: .*$', output, re.M)
summary = summaries[-1] if summaries else ''
wanted = f'test result: ok. {expected} passed; 0 failed; 0 ignored;'
if not summary.startswith(wanted):
    raise SystemExit(f'{package}: expected {expected} passed and zero skipped; got {summary or "no summary"}')
PY
done
echo 'Systemd task-scope gate passed: 21 tests ran, zero skipped'
