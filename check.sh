set -u
mode="${1:-build}"
case "$mode" in
  build)
    cargo test -p agent-file-tools --test integration --no-run 2>&1 | grep -E '^error|^warning|--> ' | head -30
    echo "done" ;;
  fmt)
    rustfmt --edition 2021 --check crates/aft/tests/integration/*.rs crates/aft/tests/helpers/mod.rs; echo "fmt exit=$?" ;;
  filter)
    cargo test -p agent-file-tools --test integration -- "$2" 2>&1 | grep -E '^test |^test result|panicked at' | head -80 ;;
  *)
    shift
    cargo test -p agent-file-tools --test integration -- --exact "$@" 2>&1 | grep -E '^test |^test result|panicked at' | head -40 ;;
esac
