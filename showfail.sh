python3 - <<'PY'
import re, sys
text = open('target/scratch/out/integration.txt', errors='replace').read()
blocks = re.findall(r'^---- (\S+) stdout ----\n(.*?)(?=\n---- |\n\nfailures:)', text, re.M | re.S)
want = sys.argv[1:] if len(sys.argv) > 1 else None
for name, body in blocks:
    if want and not any(w in name for w in want):
        continue
    print('=====', name)
    print(body[:2000])
PY
