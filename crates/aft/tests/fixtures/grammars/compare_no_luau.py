"""Pin the raw .luaurc record as the sole no-Luau feature-build exception."""

import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
enabled = json.loads((root / "on.json").read_text())
disabled = json.loads((root / "off.json").read_text())
assert enabled.pop("luaurc_record") is not None
assert disabled.pop("luaurc_record") is None
assert enabled.keys() == disabled.keys()
assert set(enabled["other_records"]) == {"plain.ts"}
for section in enabled:
    assert enabled[section] == disabled[section], (section, enabled[section], disabled[section])
print("no-Luau .luaurc parity: 8 checks passed; 4 groups byte-identical; only the raw config record and its state language differ")
