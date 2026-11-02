set -u
# Scratch driver (untracked, never committed).
git log -1 --oneline
echo "TMPDIR resolved=$(cd "${TMPDIR:-/tmp}" && pwd -P)"
echo "temp inside git: top=$(git -C "${TMPDIR:-/tmp}" rev-parse --show-toplevel 2>/dev/null || echo none)"
mkdir -p target/scratch/out
case "${1:-suites}" in
  suites)
    cargo test -p agent-file-tools --test integration > target/scratch/out/integration.txt 2>&1
    echo "integration exit=$?"
    cargo test -p agent-file-tools --bin aft > target/scratch/out/bin.txt 2>&1
    echo "bin exit=$?"
    grep -E '^test result' target/scratch/out/integration.txt target/scratch/out/bin.txt
    grep -E '^test .* \.\.\. FAILED' target/scratch/out/integration.txt target/scratch/out/bin.txt
    ;;
  failures)
    python3 - <<'PY'
import re, glob
for path in sorted(glob.glob('target/scratch/out/*.txt')):
    text = open(path, errors='replace').read()
    print('=== ' + path)
    for start in [m.start() for m in re.finditer(r'^---- ', text, re.M)]:
        print(text[start:start+2500]); print('#'*70)
PY
    ;;
  *)
    shift
    cargo test -p agent-file-tools --test integration -- --exact "$@" 2>&1 | grep -E '^test |^test result|panicked at' | head -40
    ;;
esac
