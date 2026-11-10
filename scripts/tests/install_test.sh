#!/bin/sh
# Tests scripts/install.sh against a local HTTP server and a temporary HOME.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
INSTALL="$HERE/../install.sh"
WORK=$(mktemp -d)
SERVER_PID=""
# shellcheck disable=SC2329,SC2317  # runs through the trap
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

REL="$WORK/site/releases/download/$TAG"

# Writes a fake program $2 (wi or wiki-island) to $1 that prints a version and records setup runs.
fake_program() {
  # shellcheck disable=SC2016  # $HOME and $1 expand when the fake program runs
  printf '#!/bin/sh\necho "fake-%s 9.9.9"\nif [ "${1:-}" = setup ]; then echo setup-ran >> "$HOME/setup.log"; fi\nexit 0\n' "$2" > "$1"
  chmod 755 "$1"
}

# Writes SHA256SUMS for the archive in $REL, one line in the normal two-column form.
write_manifest() {
  printf '%s  %s\n' "$(sha256_of "$REL/$ASSET")" "$ASSET" > "$REL/SHA256SUMS"
}

# Builds the fake release in $WORK/site. $1 = good|badrun|badalias.
make_release() {
  rm -rf "$WORK/site" "$WORK/pkg"
  mkdir -p "$REL" "$WORK/pkg"
  for name in wi wiki-island; do
    if [ "$1" = good ] || { [ "$1" = badalias ] && [ "$name" = wi ]; }; then
      fake_program "$WORK/pkg/$name" "$name"
    else
      printf '#!/bin/sh\nexit 3\n' > "$WORK/pkg/$name"
      chmod 755 "$WORK/pkg/$name"
    fi
  done
  echo license > "$WORK/pkg/LICENSE"
  echo readme > "$WORK/pkg/README.md"
  tar -C "$WORK/pkg" -czf "$REL/$ASSET" wi wiki-island LICENSE README.md
  write_manifest
  printf '%s\n' "$TAG" > "$WORK/site/LATEST_TAG"
}

# Replaces the release archive with one built by make_archive.py from entries $@ (after make_release).
craft_archive() {
  python3 "$HERE/make_archive.py" "$REL/$ASSET" "$@"
  write_manifest
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
# Set SEED_OLD=1 (and reset it after) so the HOME starts with an older wi and wiki-island installed.
run_install() {
  rm -rf "${WORK:?}/home"
  mkdir -p "$WORK/home"
  if [ "${SEED_OLD:-0}" = 1 ]; then
    mkdir -p "$WORK/home/.cargo/bin"
    for name in wi wiki-island; do
      printf '#!/bin/sh\necho old-%s\n' "$name" > "$WORK/home/.cargo/bin/$name"
      chmod 755 "$WORK/home/.cargo/bin/$name"
    done
  fi
  RC=0
  env -i PATH="${RUN_PATH:-$PATH}" HOME="$WORK/home" SHELL=/bin/bash TMPDIR="$WORK" \
    WI_INSTALL_BASE_URL="$URL" "$@" sh "$INSTALL" > "$WORK/out" 2>&1 || RC=$?
}

BIN="$WORK/home/.cargo/bin"
SEED_OLD=0
RUN_PATH=""
# True when both programs are still the seeded older ones and no staging files are left.
# shellcheck disable=SC2329,SC2317  # called through cond
old_pair_intact() {
  grep -qs old-wi "$BIN/wi" && grep -qs old-wiki-island "$BIN/wiki-island" \
    && [ -z "$(find "$BIN" -name '.*' -type f)" ]
}
# True when nothing was installed and no source build ran.
# shellcheck disable=SC2329,SC2317  # called through cond
nothing_installed() {
  [ ! -e "$BIN/wi" ] && [ ! -e "$BIN/wiki-island" ] && [ ! -e "$WORK/home/source.log" ]
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
if [ "$(uname -s)" = Linux ]; then
  check "verified install adds PATH to .bashrc on Linux" "$(cond grep -qF '.cargo/bin' "$WORK/home/.bashrc")"
  check "verified install adds PATH to .profile on Linux" "$(cond grep -qF '.cargo/bin' "$WORK/home/.profile")"
else
  check "verified install adds PATH to .bash_profile on macOS" "$(cond grep -qF '.cargo/bin' "$WORK/home/.bash_profile")"
fi

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

# 10. No published release: GitHub redirects releases/latest to /releases.
make_release good
rm "$WORK/site/LATEST_TAG"
echo /releases > "$WORK/site/LATEST_REDIRECT"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
rm "$WORK/site/LATEST_REDIRECT"
check "redirect to /releases falls back to the source build" "$(cond grep -q source-build "$WORK/home/source.log")"
check "redirect to /releases says no prebuilt program is published yet" "$(cond grep -q 'published yet' "$WORK/out")"
check "redirect to /releases downloads no archive" "$(cond [ "$(grep -c Downloading "$WORK/out")" -eq 0 ])"

# 11. An archive whose wi is a symbolic link to a file outside it: stop, nothing installed or run.
make_release good
mkdir -p "$WORK/outside"
printf '#!/bin/sh\necho OUTSIDE > "%s/outside/ran"\n' "$WORK" > "$WORK/outside/payload"
chmod 600 "$WORK/outside/payload"
craft_archive "link:wi:$WORK/outside/payload" "file:wiki-island:$WORK/pkg/wiki-island" \
  "file:LICENSE:$WORK/pkg/LICENSE" "file:README.md:$WORK/pkg/README.md"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
check "symlinked program exits non-zero" "$(cond [ "$RC" -ne 0 ])"
check "symlinked program installs nothing" "$(cond nothing_installed)"
check "symlinked program never runs the outside file" "$(cond [ ! -e "$WORK/outside/ran" ])"
check "symlinked program leaves the outside file's mode alone" "$(cond [ ! -x "$WORK/outside/payload" ])"

# 12. An archive with a '..' entry: stop, nothing installed, nothing written outside.
make_release good
craft_archive "file:wi:$WORK/pkg/wi" "file:wiki-island:$WORK/pkg/wiki-island" \
  "file:LICENSE:$WORK/pkg/LICENSE" "file:README.md:$WORK/pkg/README.md" "file:../escaped:$WORK/pkg/LICENSE"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
check "'..' entry exits non-zero" "$(cond [ "$RC" -ne 0 ])"
check "'..' entry installs nothing" "$(cond nothing_installed)"
check "'..' entry says so in a plain sentence" "$(cond grep -q 'unexpected files' "$WORK/out")"

# 13. An archive with an extra entry: stop, nothing installed.
make_release good
craft_archive "file:wi:$WORK/pkg/wi" "file:wiki-island:$WORK/pkg/wiki-island" \
  "file:LICENSE:$WORK/pkg/LICENSE" "file:README.md:$WORK/pkg/README.md" "file:unexpected:$WORK/pkg/LICENSE"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
check "extra entry exits non-zero" "$(cond [ "$RC" -ne 0 ])"
check "extra entry installs nothing" "$(cond nothing_installed)"

# 14. The second rename fails: the older pair is restored and no staging file remains.
make_release good
REAL_MV=$(command -v mv)
mkdir -p "$WORK/shim"
cat > "$WORK/shim/mv" <<SHIM
#!/bin/sh
for arg in "\$@"; do
  case "\$arg" in */.wiki-island.new.*) echo "mv: injected failure" >&2; exit 1 ;; esac
done
exec "$REAL_MV" "\$@"
SHIM
chmod 755 "$WORK/shim/mv"
SEED_OLD=1 RUN_PATH="$WORK/shim:$PATH"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
SEED_OLD=0 RUN_PATH=""
check "second rename failure exits non-zero" "$(cond [ "$RC" -ne 0 ])"
check "second rename failure restores the older pair" "$(cond old_pair_intact)"

# 15. A folder sits where wiki-island belongs: stop before replacing anything.
make_release good
rm -rf "${WORK:?}/home"
mkdir -p "$BIN/wiki-island"
printf '#!/bin/sh\necho old-wi\n' > "$BIN/wi"
chmod 755 "$BIN/wi"
RC=0
env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash TMPDIR="$WORK" WI_INSTALL_BASE_URL="$URL" \
  WI_INSTALL_SOURCE_CMD="$SOURCE_CMD" sh "$INSTALL" > "$WORK/out" 2>&1 || RC=$?
check "folder destination exits non-zero" "$(cond [ "$RC" -ne 0 ])"
check "folder destination keeps the older wi" "$(cond grep -q old-wi "$BIN/wi")"
check "folder destination leaves the folder empty" "$(cond [ -z "$(ls -A "$BIN/wiki-island")" ])"

# 16. wiki-island in the archive cannot run: source build instead of installing it.
make_release badalias
SEED_OLD=1
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
SEED_OLD=0
check "failing wiki-island falls back to the source build" "$(cond grep -q source-build "$WORK/home/source.log")"
check "failing wiki-island is not installed" "$(cond grep -q built-from-source "$BIN/wiki-island")"

# 17. A checksum list with CRLF line endings is accepted.
make_release good
printf '%s  %s\r\n' "$(sha256_of "$REL/$ASSET")" "$ASSET" > "$REL/SHA256SUMS"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
check "CRLF checksum list installs" "$(cond [ "$RC" -eq 0 ])"
check "CRLF checksum list installs the release" "$(cond grep -q fake-wi "$BIN/wi")"

# 18. A matching line with an extra field is malformed: stop, nothing installed.
make_release good
printf '%s  %s junk\n' "$(sha256_of "$REL/$ASSET")" "$ASSET" > "$REL/SHA256SUMS"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
check "extra manifest field exits non-zero" "$(cond [ "$RC" -ne 0 ])"
check "extra manifest field installs nothing" "$(cond nothing_installed)"

# 19. Upper-case hex digests and the binary-mode '*' marker are accepted.
make_release good
printf '%s *%s\n' "$(sha256_of "$REL/$ASSET" | tr 'a-f' 'A-F')" "$ASSET" > "$REL/SHA256SUMS"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
check "upper-case digest installs" "$(cond [ "$RC" -eq 0 ])"
check "upper-case digest installs the release" "$(cond grep -q fake-wi "$BIN/wi")"

# 20. Two matching lines: stop, nothing installed.
make_release good
write_manifest
cat "$REL/SHA256SUMS" "$REL/SHA256SUMS" > "$WORK/dup" && mv "$WORK/dup" "$REL/SHA256SUMS"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
check "duplicate manifest line exits non-zero" "$(cond [ "$RC" -ne 0 ])"
check "duplicate manifest line installs nothing" "$(cond nothing_installed)"

# 21. A verified install over an older pair replaces both and leaves no staging files.
make_release good
SEED_OLD=1
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
SEED_OLD=0
check "upgrade exits 0" "$(cond [ "$RC" -eq 0 ])"
check "upgrade replaces both programs" "$(cond grep -q fake-wiki-island "$BIN/wiki-island")"
check "upgrade leaves no staging files" "$(cond [ -z "$(find "$BIN" -name '.*' -type f)" ])"

# 22. An archive larger than the size limit (lowered for the test): stop, nothing installed, no
# source build. Then the same without a Content-Length header, and an archive exactly at the limit.
make_release good
ASSET_SIZE=$(wc -c < "$REL/$ASSET" | tr -d ' ')
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD" WI_INSTALL_MAX_ASSET_BYTES=$((ASSET_SIZE - 1))
check "oversized archive exits non-zero" "$(cond [ "$RC" -ne 0 ])"
check "oversized archive installs nothing" "$(cond nothing_installed)"
check "oversized archive says so in a plain sentence" "$(cond grep -q 'larger than any release should be' "$WORK/out")"
: > "$WORK/site/NO_LENGTH"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD" WI_INSTALL_MAX_ASSET_BYTES=$((ASSET_SIZE - 1))
rm "$WORK/site/NO_LENGTH"
check "oversized archive without a stated size exits non-zero" "$(cond [ "$RC" -ne 0 ])"
check "oversized archive without a stated size installs nothing" "$(cond nothing_installed)"
check "oversized archive without a stated size says so" "$(cond grep -q 'larger than any release should be' "$WORK/out")"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD" WI_INSTALL_MAX_ASSET_BYTES="$ASSET_SIZE"
check "archive exactly at the limit installs" "$(cond [ "$RC" -eq 0 ])"
check "archive exactly at the limit installs the release" "$(cond grep -q fake-wi "$BIN/wi")"

# 22b. curl writes more than the limit and then fails with an error other than 63: still a
# refusal, not a source build. A fake curl stands in for the asset download only.
make_release good
ASSET_SIZE=$(wc -c < "$REL/$ASSET" | tr -d ' ')
REAL_CURL=$(command -v curl)
mkdir -p "$WORK/fakebin"
cat > "$WORK/fakebin/curl" <<FAKE
#!/bin/sh
out=""
prev=""
for arg in "\$@"; do
  [ "\$prev" = "-o" ] && out=\$arg
  prev=\$arg
  case "\$arg" in
    *.tar.gz) head -c $((ASSET_SIZE + 10)) /dev/zero > "\$out"; exit 56 ;;
  esac
done
exec "$REAL_CURL" "\$@"
FAKE
chmod 755 "$WORK/fakebin/curl"
RUN_PATH="$WORK/fakebin:$PATH"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD" WI_INSTALL_MAX_ASSET_BYTES=$((ASSET_SIZE - 1))
RUN_PATH=""
check "oversized partial download with a non-63 error exits non-zero" "$(cond [ "$RC" -ne 0 ])"
check "oversized partial download with a non-63 error installs nothing" "$(cond nothing_installed)"
check "oversized partial download with a non-63 error says so" "$(cond grep -q 'larger than any release should be' "$WORK/out")"
# An override of 0 is ignored, so the default limit applies and the archive installs.
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD" WI_INSTALL_MAX_ASSET_BYTES=0
check "a limit override of 0 keeps the default limit" "$(cond grep -q fake-wi "$BIN/wi")"

# 23. A checksum list larger than 64 KiB: stop, nothing installed, no source build.
make_release good
awk 'BEGIN { for (i = 0; i < 1100; i++) printf "%064d  padding-%d\n", 0, i }' >> "$REL/SHA256SUMS"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
check "oversized checksum list exits non-zero" "$(cond [ "$RC" -ne 0 ])"
check "oversized checksum list installs nothing" "$(cond nothing_installed)"
check "oversized checksum list says so in a plain sentence" "$(cond grep -q 'checksum list is larger' "$WORK/out")"

# True when both programs are the new release and no staging files are left.
# shellcheck disable=SC2329,SC2317  # called through cond
new_pair_intact() {
  grep -qs fake-wi "$BIN/wi" && grep -qs fake-wiki-island "$BIN/wiki-island" \
    && [ -z "$(find "$BIN" -name '.*' -type f)" ]
}
# True when the installer left no extraction folder behind.
# shellcheck disable=SC2329,SC2317  # called through cond
no_extraction_left() {
  [ -z "$(find "$WORK" -mindepth 1 -maxdepth 1 -name 'tmp.*')" ]
}

# True when the output does not claim the previous programs were kept.
# shellcheck disable=SC2329,SC2317  # called through cond
no_kept_claim() {
  ! grep -q 'were kept' "$WORK/out"
}

# Writes shim $1/$2 that fails (exit 1) when its joined arguments match case pattern $3, else runs
# the real $2.
fail_shim() {
  mkdir -p "$1"
  cat > "$1/$2" <<SHIM
#!/bin/sh
case "\$*" in $3) echo "$2: injected failure" >&2; exit 1 ;; esac
exec "$(command -v "$2")" "\$@"
SHIM
  chmod 755 "$1/$2"
}

# 24. Every move onto wiki-island fails (a persistent destination error): the older pair is kept.
make_release good
rm -rf "$WORK/shim-persist"
fail_shim "$WORK/shim-persist" mv '*/wiki-island'
SEED_OLD=1 RUN_PATH="$WORK/shim-persist:$PATH"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
SEED_OLD=0 RUN_PATH=""
check "persistent rename failure exits non-zero" "$(cond [ "$RC" -ne 0 ])"
check "persistent rename failure keeps the older pair" "$(cond old_pair_intact)"
check "persistent rename failure says the previous programs were kept" "$(cond grep -q 'previous ones were kept' "$WORK/out")"

# 25. The restore also fails: the installer says the installation is incomplete and exits non-zero.
make_release good
rm -rf "$WORK/shim-restore"
fail_shim "$WORK/shim-restore" mv '*/wiki-island | */.wi.old.*'
SEED_OLD=1 RUN_PATH="$WORK/shim-restore:$PATH"
run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
SEED_OLD=0 RUN_PATH=""
check "failed restore exits non-zero" "$(cond [ "$RC" -ne 0 ])"
check "failed restore says the installation is incomplete" "$(cond grep -q 'incomplete' "$WORK/out")"
check "failed restore does not claim the previous programs were kept" "$(cond no_kept_claim)"
check "failed restore leaves both names runnable" "$(cond [ -x "$BIN/wi" ] && [ -x "$BIN/wiki-island" ])"

# 26. TERM arrives at each step of the replacement: both names stay runnable as one pair and
# nothing is left behind. The shim signals the installer once, then lets the step itself run.
for step in "cp:*/.wi.new.*" "cp:*/.wiki-island.new.*" "cp:*/.wi.old.*" "cp:*/.wiki-island.old.*" \
  "mv:*/.wi.new.*" "mv:*/.wiki-island.new.*" "rm:*/.wi.old.*"; do
  tool=${step%%:*}
  pattern=${step#*:}
  make_release good
  rm -rf "$WORK/shim-term" "$WORK/term-sent"
  mkdir -p "$WORK/shim-term"
  cat > "$WORK/shim-term/$tool" <<SHIM
#!/bin/sh
for arg in "\$@"; do
  case "\$arg" in $pattern)
    if [ ! -e "$WORK/term-sent" ]; then : > "$WORK/term-sent"; kill -TERM "\$PPID"; fi ;;
  esac
done
exec "$(command -v "$tool")" "\$@"
SHIM
  chmod 755 "$WORK/shim-term/$tool"
  SEED_OLD=1 RUN_PATH="$WORK/shim-term:$PATH"
  run_install WI_INSTALL_SOURCE_CMD="$SOURCE_CMD"
  SEED_OLD=0 RUN_PATH=""
  check "TERM at $tool $pattern was delivered" "$(cond [ -e "$WORK/term-sent" ])"
  check "TERM at $tool $pattern exits non-zero" "$(cond [ "$RC" -ne 0 ])"
  check "TERM at $tool $pattern leaves one runnable pair" "$(cond eval 'old_pair_intact || new_pair_intact')"
  check "TERM at $tool $pattern leaves no extraction folder" "$(cond no_extraction_left)"
done

[ "$FAILED" -eq 0 ] && echo "all install tests passed"
exit "$FAILED"
