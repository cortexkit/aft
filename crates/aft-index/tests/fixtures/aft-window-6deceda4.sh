#!/usr/bin/env bash
# AFT window: stop AFT, remove its old call-graph stores (AFT's own script, dry
# run first), place AFT card 6deceda4, start AFT.
# Runs detached, because stopping AFT ends the tools of every agent session,
# including the one that started this. Usage: aft-window-6deceda4.sh <HH:MM:SS UTC start>
set -u
START=$1
SUBC=~/Work/Projects/CortexKit/subconscious
STG=~/.local/share/cortexkit/staging
BIN=~/.local/share/cortexkit/bin
LOG=$STG/aft-window-6deceda4.log
exec >>"$LOG" 2>&1
ts() { date -u +%H:%M:%SZ; }
aft_pid() {
  ck --json provenance aft 2>/dev/null | python3 -c \
    'import json,sys; m=json.load(sys.stdin)["modules"][0]; print((m.get("daemon_observed") or {}).get("pid") or "")'
}
echo "=== $(ts) waiting for $START"
while [ "$(date -u +%H:%M:%S)" \< "$START" ]; do sleep 5; done
echo "=== $(ts) free before: $(df -g / | awk 'NR==2{print $4" GiB"}')"

# The stop is confirmed on the exact process the daemon ran (pid plus its
# start time), never by matching command lines: detached bash tasks AFT
# launched keep running by design and must not count as AFT.
OLD=$(aft_pid)
OLD_START=$( [ -n "$OLD" ] && ps -o lstart= -p "$OLD" 2>/dev/null )
echo "=== $(ts) stop aft (pid=$OLD started $OLD_START)"
ck module stop aft 2>&1 | tail -2
SKIP_REMOVAL=1
for i in $(seq 1 60); do
  now=$( [ -n "$OLD" ] && ps -o lstart= -p "$OLD" 2>/dev/null )
  state=$(ck module list 2>/dev/null | awk '$1=="aft"{print $2}')
  if [ -n "$OLD" ] && [ "$now" != "$OLD_START" ] && [ "$state" = "stopped" ]; then
    SKIP_REMOVAL=0; break
  fi
  sleep 2
done
if [ "$SKIP_REMOVAL" = 0 ]; then
  echo "$(ts) aft stopped: pid $OLD gone, module state stopped"
else
  echo "$(ts) could not confirm the stop (pid=$OLD, state=$state); skipping the removal, placing and starting anyway"
fi

if [ "$SKIP_REMOVAL" = 0 ]; then
  R=~/Work/Projects/CortexKit/aft/.cortexkit/scripts/legacy-callgraph-removal.py
  F="be627d40119a995e 90ff783f3f4c5cf2 e274ab0872bb490b c5fd16d858c5f6c5"
  echo "=== $(ts) removal dry run"
  python3 "$R" $F 2>&1 | tail -25
  echo "dry run rc=${PIPESTATUS[0]}"
  echo "=== $(ts) removal --apply"
  python3 "$R" --apply $F 2>&1 | tail -25
  echo "apply rc=${PIPESTATUS[0]}"
fi

echo "=== $(ts) place aft card"
"$SUBC/scripts/fleet/place-module.sh" --module aft --staged "$STG/ck-aft.6deceda4a190ccd2" \
  --marker kill_scope --control status_push_list_omitted --no-restart --place 2>&1 | tail -12
echo "=== $(ts) start aft"
ck module start aft 2>&1 | tail -2
for i in $(seq 1 90); do
  [ "$(ck module list 2>/dev/null | awk '$1=="aft"{print $2}')" = "running" ] && [ -n "$(aft_pid)" ] && break
  sleep 2
done
pid=$(aft_pid)
echo "$(ts) aft pid=$pid inode proc=$(lsof -p "$pid" -a -d txt -Fi 2>/dev/null | grep -m1 '^i' | cut -c2-) disk=$(stat -f %i "$BIN/ck-aft")"
"$BIN/ck-aft" --version 2>&1 | head -1
ck health aft 2>&1 | head -2
echo "=== $(ts) free after: $(df -g / | awk 'NR==2{print $4" GiB"}')"
echo "=== $(ts) WINDOW DONE"
