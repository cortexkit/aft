set -e; S=~/fixture-scratch; cd ~/fixture-project && git worktree add -q --detach $S/fixture-s14 HEAD && cd $S/fixture-s14 && git log --oneline -1 && cat > .gate-s14.sh <<'EOF'
#!/usr/bin/env bash
set -u
echo "== s14 $(git log --oneline -1 | cut -c1-60)"
cargo fmt --all --check > /tmp/fmt.log 2>&1; echo "fmt exit $?"
cargo nextest run -p fixture-core-store --no-fail-fast > /tmp/store.log 2>&1; grep -a -E '^\s+(FAIL|TIMEOUT)|Summary' /tmp/store.log | sort -u | head
cargo nextest run -p fixture-core-module --lib --bins --no-fail-fast > /tmp/lib.log 2>&1; grep -a -E '^\s+(FAIL|TIMEOUT)|Summary' /tmp/lib.log | sort -u | head -20
cargo nextest run -p fixture-core-module --test it --no-fail-fast > /tmp/it.log 2>&1; grep -a -E '^\s+(FAIL|TIMEOUT)|Summary' /tmp/it.log | sort -u | head -20
cargo nextest run -p fixture-core-module --test e2e --run-ignored all -j 1 --no-fail-fast -E 'test(/^real_daemon_e2e::real_subc_core_serves_peer_messages_on_the_same_surface$/) | test(/^manager_rooms_e2e::/) | test(/^agent_registry_e2e::.*peer/) | test(=agent_registry_e2e::legacy_prepared_delivery_requires_acceptance_before_bus_production) | test(/^wake_delivery_e2e::peer_/) | test(/^manager_cadence_loops_e2e::peer_/)' > /tmp/e2e.log 2>&1
grep -a -E '^\s+(FAIL|TIMEOUT)|Summary' /tmp/e2e.log | sort -u | head -40
grep -a -n -A3 'panicked at' /tmp/e2e.log | cut -c1-240 | head -60
# Break 1: a digest mismatch no longer quarantines.
F=crates/fixture-core-module/src/bus/peer_consumer.rs
cp $F /tmp/pc.orig
python3 - <<'PY'
p='crates/fixture-core-module/src/bus/peer_consumer.rs';t=open(p).read()
n=t.replace('reason = Some("digest_mismatch");','reason = None;',1); assert n!=t; open(p,'w').write(n)
PY
grep -n 'reason = None;' $F | head -2
cargo nextest run -p fixture-core-module --lib --no-fail-fast -E 'test(/peer_consumer/)' > /tmp/b1.log 2>&1; echo "BREAK1:"; grep -a -E '^\s+(FAIL|TIMEOUT)|Summary' /tmp/b1.log | sort -u
cp /tmp/pc.orig $F
# Break 2: busy previews count as reads.
G=crates/fixture-core-store/src/inbox/adopt_c.rs
cp $G /tmp/ac.orig
python3 - <<'PY'
p='crates/fixture-core-store/src/inbox/adopt_c.rs';t=open(p).read()
n=t.replace('if full_ids.iter().any(|full| full == id) {','if true {',1); assert n!=t; open(p,'w').write(n)
PY
grep -n 'if true {' $G | head -2
cargo nextest run -p fixture-core-module --lib --no-fail-fast -E 'test(/peer_consumer/)' > /tmp/b2.log 2>&1; echo "BREAK2:"; grep -a -E '^\s+(FAIL|TIMEOUT)|Summary' /tmp/b2.log | sort -u
cp /tmp/ac.orig $G
git diff --stat | tail -1; echo "restored: $(git diff --quiet && echo yes || echo NO)"
EOF
chmod +x .gate-s14.sh && sed -e '/^run fixture-lanes-s2/d' -e 's#^wait#run fixture-s14 .gate-s14.sh 10 subconscious insula commons broca\nwait#' /tmp/run-lanes-s2b.sh > /tmp/run-s14.sh && chmod +x /tmp/run-s14.sh && grep -n '^run \|^wait\|nats' /tmp/run-s14.sh
