#!/bin/sh
# Proves that the library crates on the capture, search and MCP path have no network client
# in their normal (non-dev, non-build) dependency trees, on the host and on Windows.
#
# It fails when a listed network, TLS or socket crate appears in a tree, or when tokio has its
# `net` feature enabled. It proves nothing about runtime behavior of the whole binary.
#
# Run from anywhere: sh scripts/check-offline-tree.sh
# Tests only: WI_OFFLINE_CHECK_CRATES overrides the crates to inspect (space separated), to
# show that the check fails on trees that do contain an HTTP client, such as wi-notion.
set -eu

cd "$(dirname "$0")/.."

CRATES="${WI_OFFLINE_CHECK_CRATES:-wi-mcp wi-store wi-index}"
TARGETS="host x86_64-pc-windows-msvc"
FORBIDDEN="reqwest ureq hyper hyper-util h2 rustls native-tls openssl openssl-sys schannel curl curl-sys mio socket2 tungstenite isahc attohttpc minreq webpki-roots"

fail_count=0

# The cargo flag that selects a target; nothing for the host.
target_flag() {
  if [ "$1" = host ]; then
    return 0
  fi
  echo "--target $1"
}

# Runs cargo tree for one crate and target; the rest are cargo tree flags.
tree() {
  crate=$1
  target=$2
  shift 2
  if [ "$target" = host ]; then
    cargo tree -p "$crate" "$@"
  else
    cargo tree -p "$crate" "$@" --target "$target"
  fi
}

for crate in $CRATES; do
  for target in $TARGETS; do
    bad=0
    names=$(tree "$crate" "$target" -e normal --prefix none) || {
      echo "error: cargo tree failed for $crate on $target" >&2
      exit 2
    }
    names=$(printf '%s\n' "$names" | awk '{print $1}' | sort -u)
    for name in $FORBIDDEN; do
      if printf '%s\n' "$names" | grep -qx -- "$name"; then
        echo "FAIL $crate $target: dependency tree contains network crate '$name'. See: cargo tree -p $crate -e normal $(target_flag "$target") -i $name" >&2
        bad=1
      fi
    done
    features=$(tree "$crate" "$target" -e features) || {
      echo "error: cargo tree -e features failed for $crate on $target" >&2
      exit 2
    }
    if printf '%s\n' "$features" | grep -q 'tokio feature "net"'; then
      echo "FAIL $crate $target: tokio has its 'net' feature enabled. See: cargo tree -p $crate -e features $(target_flag "$target") -i tokio" >&2
      bad=1
    fi
    if [ "$bad" -eq 0 ]; then
      echo "ok $crate $target"
    else
      fail_count=$((fail_count + 1))
    fi
  done
done

if [ "$fail_count" -ne 0 ]; then
  echo "offline tree check failed for $fail_count tree(s)." >&2
  exit 1
fi
