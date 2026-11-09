#!/bin/sh
# Tests scripts/install.sh against a local HTTP server and a temporary HOME.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
INSTALL="$HERE/../install.sh"
WORK=$(mktemp -d)
SERVER_PID=""
# shellcheck disable=SC2329  # runs through the trap
cleanup() {
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "${WORK:?}"
}
trap cleanup EXIT

case "$(uname -s)/$(uname -m)" in
  Darwin/arm64) TARGET=aarch64-apple-darwin ;;
  Darwin/x86_64) TARGET=x86_64-apple-darwin ;;
  Linux/aarch64 | Linux/arm64) TARGET=aarch64-unknown-linux-gnu ;;
  Linux/x86_64) TARGET=x86_64-unknown-linux-gnu ;;
  *) echo "unsupported test host"; exit 1 ;;
esac
TAG=v9.9.9
ASSET="wiki-island-$TAG-$TARGET.tar.gz"

sha256_of() {
  if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}'; else sha256sum "$1" | awk '{print $1}'; fi
}

# Builds the fake release in $WORK/site. $1 = good|badrun.
make_release() {
  rm -rf "$WORK/site" "$WORK/pkg"
  mkdir -p "$WORK/site/releases/download/$TAG" "$WORK/pkg"
  for name in wi wiki-island; do
    if [ "$1" = good ]; then
      # shellcheck disable=SC2016  # $HOME and $1 expand when the fake program runs
      printf '#!/bin/sh\necho "fake-%s 9.9.9"\nif [ "${1:-}" = setup ]; then echo setup-ran >> "$HOME/setup.log"; fi\nexit 0\n' "$name" > "$WORK/pkg/$name"
    else
      printf '#!/bin/sh\nexit 3\n' > "$WORK/pkg/$name"
    fi
    chmod 755 "$WORK/pkg/$name"
  done
  tar -C "$WORK/pkg" -czf "$WORK/site/releases/download/$TAG/$ASSET" wi wiki-island
  printf '%s  %s\n' "$(sha256_of "$WORK/site/releases/download/$TAG/$ASSET")" "$ASSET" \
    > "$WORK/site/releases/download/$TAG/SHA256SUMS"
  printf '%s\n' "$TAG" > "$WORK/site/LATEST_TAG"
}

PORT_FILE="$WORK/port"
python3 "$HERE/release_server.py" "$WORK/site-root" "$PORT_FILE" &
SERVER_PID=$!
# The server serves a symlink so each case can rebuild $WORK/site freely.
mkdir -p "$WORK/site"
rm -rf "$WORK/site-root"
ln -s "$WORK/site" "$WORK/site-root"
i=0
while [ ! -s "$PORT_FILE" ] && [ "$i" -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
[ -s "$PORT_FILE" ] || { echo "server did not start"; exit 1; }
URL="http://127.0.0.1:$(cat "$PORT_FILE")"

FAILED=0
check() {
  if [ "$2" = ok ]; then echo "ok   $1"; else echo "FAIL $1"; FAILED=1; fi
}
cond() { if "$@"; then echo ok; else echo no; fi; }

# Runs install.sh with a fresh HOME. Output in $WORK/out, exit status in $RC.
run_install() {
  rm -rf "${WORK:?}/home"
  mkdir -p "$WORK/home"
  RC=0
  env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash TMPDIR="$WORK" \
    WI_INSTALL_BASE_URL="$URL" "$@" sh "$INSTALL" > "$WORK/out" 2>&1 || RC=$?
}

# shellcheck disable=SC2016  # expands inside install.sh's HOME
SOURCE_CMD='mkdir -p "$HOME/.cargo/bin"; printf "#!/bin/sh\necho built-from-source\n" > "$HOME/.cargo/bin/wi"; cp "$HOME/.cargo/bin/wi" "$HOME/.cargo/bin/wiki-island"; chmod 755 "$HOME/.cargo/bin/wi" "$HOME/.cargo/bin/wiki-island"; echo source-build >> "$HOME/source.log"'

# 1. Verified install.
make_release good
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
check "verified install exits 0" "$(cond [ "$RC" -eq 0 ])"
check "verified install puts wi in place" "$(cond [ -x "$WORK/home/.cargo/bin/wi" ])"
check "verified install puts wiki-island in place" "$(cond [ -x "$WORK/home/.cargo/bin/wiki-island" ])"
check "verified install uses the release, not the source build" "$(cond [ ! -e "$WORK/home/source.log" ])"
check "verified install runs wi setup" "$(cond grep -q setup-ran "$WORK/home/setup.log")"
check "verified install adds PATH to .bashrc and .profile on Linux" \
  "$(if [ "$(uname -s)" = Linux ]; then cond grep -qF '.cargo/bin' "$WORK/home/.bashrc" ; else cond grep -qF '.cargo/bin' "$WORK/home/.bash_profile"; fi)"

# 2. Running twice adds the PATH line once.
RC=0
env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash TMPDIR="$WORK" WI_INSTALL_BASE_URL="$URL" sh "$INSTALL" > "$WORK/out" 2>&1 || RC=$?
if [ "$(uname -s)" = Linux ]; then PROFILE_FILE="$WORK/home/.bashrc"; else PROFILE_FILE="$WORK/home/.bash_profile"; fi
check "second run exits 0" "$(cond [ "$RC" -eq 0 ])"
check "PATH line appears once" "$(cond [ "$(grep -cF '.cargo/bin' "$PROFILE_FILE")" -eq 1 ])"

# 3. Checksum mismatch: non-zero, nothing installed, no source build.
make_release good
printf '%s  %s\n' "0000000000000000000000000000000000000000000000000000000000000000" "$ASSET" \
  > "$WORK/site/releases/download/$TAG/SHA256SUMS"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
check "checksum mismatch exits non-zero" "$(cond [ "$RC" -ne 0 ])"
check "checksum mismatch installs nothing" "$(cond [ ! -e "$WORK/home/.cargo/bin/wi" ])"
check "checksum mismatch does not fall back to source" "$(cond [ ! -e "$WORK/home/source.log" ])"
check "checksum mismatch says so in plain words" "$(cond grep -q 'does not match its checksum' "$WORK/out")"

# 4. Malformed manifest (no entry for the asset): non-zero, nothing installed.
make_release good
printf 'abc  other-file.tar.gz\n' > "$WORK/site/releases/download/$TAG/SHA256SUMS"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
check "manifest without the asset exits non-zero" "$(cond [ "$RC" -ne 0 ])"
check "manifest without the asset installs nothing" "$(cond [ ! -e "$WORK/home/.cargo/bin/wi" ])"

# 5. Missing asset (404): source build.
make_release good
rm "$WORK/site/releases/download/$TAG/$ASSET"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
check "missing asset falls back to the source build" "$(cond grep -q source-build "$WORK/home/source.log")"
check "missing asset exits 0" "$(cond [ "$RC" -eq 0 ])"

# 6. No release at all (latest answers 404): source build.
make_release good
rm "$WORK/site/LATEST_TAG"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
check "no release falls back to the source build" "$(cond grep -q source-build "$WORK/home/source.log")"

# 7. Downloaded wi cannot run: source build replaces it.
make_release badrun
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
check "wi --version failure falls back to the source build" "$(cond grep -q source-build "$WORK/home/source.log")"
check "fallback leaves the source-built wi" "$(cond grep -q built-from-source "$WORK/home/.cargo/bin/wi")"

# 8. Forced source build.
make_release good
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD" WI_INSTALL_FROM_SOURCE=1
check "WI_INSTALL_FROM_SOURCE=1 skips the release" "$(cond grep -q source-build "$WORK/home/source.log")"

# 9. Server unreachable: source build.
make_release good
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD" WI_INSTALL_BASE_URL="http://127.0.0.1:1"
check "unreachable server falls back to the source build" "$(cond grep -q source-build "$WORK/home/source.log")"

[ "$FAILED" -eq 0 ] && echo "all install tests passed"
exit "$FAILED"
