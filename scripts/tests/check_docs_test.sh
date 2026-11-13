#!/bin/sh
# Tests scripts/check-docs.sh on fixture trees: a clean tree passes; a broken file link, a broken
# anchor and a docs/VALIDATION.md entry that names a missing test each fail with a message that
# names the file and line. Fixtures live in a temporary directory that is removed on exit.
# Run from anywhere: sh scripts/tests/check_docs_test.sh
set -eu

script="$(cd "$(dirname "$0")/.." && pwd)/check-docs.sh"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/wi-check-docs-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

failed=0

# Builds a clean fixture tree in $1.
make_tree() {
  mkdir -p "$1/docs" "$1/crates/wi-demo/tests"
  cat >"$1/README.md" <<'MD'
# Demo

See [the design](docs/DESIGN.md#goals), [setup](#set-up) and [a folder](docs).
A link in code is ignored: `[x](missing.md)`.

```md
[ignored](also-missing.md)
```

## Set up

## Repeat

## Repeat

[second repeat](#repeat-1)
MD
  cat >"$1/docs/DESIGN.md" <<'MD'
# Design

## Goals

Back to the [readme](../README.md#demo).
MD
  cat >"$1/docs/VALIDATION.md" <<'MD'
# Validation

- Reads work (`crates/wi-demo/tests/read.rs`, `reads_a_note_back_after_writing_it`).
- Short path form (`wi-demo/tests/read.rs`) and a prefix (`reads_a_note_*`).
- A name with no file is found anywhere under crates: `reads_a_note_back_after_writing_it`.
MD
  cat >"$1/crates/wi-demo/tests/read.rs" <<'RS'
#[test]
fn reads_a_note_back_after_writing_it() {}
RS
}

# Runs the script on the tree $1; output goes to $tmp/out, status to $tmp/rc.
run_check() {
  set +e
  WI_DOCS_ROOT=$1 sh "$script" >"$tmp/out" 2>&1
  echo $? >"$tmp/rc"
  set -e
}

expect_pass() {
  name=$1
  dir=$2
  run_check "$dir"
  if [ "$(cat "$tmp/rc")" = 0 ] && [ ! -s "$tmp/out" ]; then
    echo "ok $name"
  else
    echo "FAIL $name: expected a silent pass"
    cat "$tmp/out"
    failed=1
  fi
}

expect_fail() {
  name=$1
  dir=$2
  needle=$3
  run_check "$dir"
  if [ "$(cat "$tmp/rc")" = 1 ] && grep -Fq -- "$needle" "$tmp/out"; then
    echo "ok $name"
  else
    echo "FAIL $name: expected exit 1 with '$needle'"
    cat "$tmp/out"
    failed=1
  fi
}

make_tree "$tmp/clean"
expect_pass "clean tree passes" "$tmp/clean"

make_tree "$tmp/link"
printf '\nSee [gone](docs/GONE.md).\n' >>"$tmp/link/docs/DESIGN.md"
expect_fail "broken file link fails" "$tmp/link" "docs/DESIGN.md:7: broken link"

make_tree "$tmp/anchor"
printf '\nSee [nowhere](DESIGN.md#no-such-heading).\n' >>"$tmp/anchor/docs/DESIGN.md"
expect_fail "broken anchor fails" "$tmp/anchor" "docs/DESIGN.md:7: broken anchor"

make_tree "$tmp/dup"
printf '\nSee [third](../README.md#repeat-2).\n' >>"$tmp/dup/docs/DESIGN.md"
expect_fail "missing duplicate-heading suffix fails" "$tmp/dup" "broken anchor"

make_tree "$tmp/test"
# shellcheck disable=SC2016
printf -- '- Gone (`crates/wi-demo/tests/read.rs`, `deleted_test_that_nobody_wrote`).\n' >>"$tmp/test/docs/VALIDATION.md"
expect_fail "missing test name fails" "$tmp/test" "docs/VALIDATION.md:6: test deleted_test_that_nobody_wrote"

make_tree "$tmp/file"
# shellcheck disable=SC2016
printf -- '- Gone (`wi-demo/tests/missing.rs`).\n' >>"$tmp/file/docs/VALIDATION.md"
expect_fail "missing test file fails" "$tmp/file" "cited file wi-demo/tests/missing.rs does not exist"

exit "$failed"
