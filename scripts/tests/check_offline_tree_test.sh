#!/bin/sh
# Tests scripts/check-offline-tree.sh with a fake `cargo` first on PATH. The fake prints the
# dependency names in $FAKE_TREE_NAMES for `cargo tree --prefix none` and the lines in
# $FAKE_TREE_FEATURES for `cargo tree -e normal,features`. The script must pass on a clean tree
# and fail when a network crate, or the `net` feature of tokio or mio, appears.
# Run from anywhere: sh scripts/tests/check_offline_tree_test.sh
set -eu

script="$(cd "$(dirname "$0")/.." && pwd)/check-offline-tree.sh"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/wi-offline-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

mkdir "$tmp/bin"
cat >"$tmp/bin/cargo" <<'SH'
#!/bin/sh
case "$*" in
  *features*) printf '%s\n' "${FAKE_TREE_FEATURES:-}" ;;
  *) printf '%s\n' "${FAKE_TREE_NAMES:-}" ;;
esac
SH
chmod +x "$tmp/bin/cargo"

failed=0

# run_case NAME EXPECTED_STATUS NAMES FEATURES NEEDLE
run_case() {
  name=$1
  want=$2
  set +e
  PATH="$tmp/bin:$PATH" WI_OFFLINE_CHECK_CRATES=demo FAKE_TREE_NAMES=$3 FAKE_TREE_FEATURES=$4 \
    sh "$script" >"$tmp/out" 2>&1
  got=$?
  set -e
  if [ "$got" = "$want" ] && { [ -z "$5" ] || grep -Fq -- "$5" "$tmp/out"; }; then
    echo "ok $name"
  else
    echo "FAIL $name: expected status $want containing '$5', got $got"
    cat "$tmp/out"
    failed=1
  fi
}

clean_names='demo v0.1.0
serde v1.0.0
mio v1.0.0'
clean_features='demo v0.1.0
mio v1.0.0
mio feature "default"
mio feature "os-poll"'

run_case "clean tree passes" 0 "$clean_names" "$clean_features" "ok demo"
run_case "network crate fails" 1 "$clean_names
reqwest v0.12.0" "$clean_features" "network crate 'reqwest'"
run_case "tls crate fails" 1 "$clean_names
rustls v0.23.0" "$clean_features" "network crate 'rustls'"
run_case "mio net feature fails" 1 "$clean_names" "$clean_features
mio feature \"net\"" "mio has its 'net' feature enabled"
run_case "tokio net feature fails" 1 "$clean_names" "tokio feature \"net\"" "tokio has its 'net' feature enabled"

exit "$failed"
