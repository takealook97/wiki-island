#!/bin/sh
# wiki-island installer for macOS and Linux. Safe to run again. Needs no administrator rights.
#
# It installs the prebuilt `wi` and `wiki-island` programs from the latest GitHub release after
# checking their SHA-256 checksum, and builds from source when no usable release exists.
#
# Environment variables:
#   WI_INSTALL_BASE_URL     Repository URL to download from (default https://github.com/takealook97/wiki-island).
#   WI_INSTALL_FROM_SOURCE  Set to 1 to skip the release download and build from source.
#   WI_INSTALL_SOURCE_CMD   Tests only: a shell command that replaces the source build. It must
#                           put working `wi` and `wiki-island` programs into $HOME/.cargo/bin.
set -eu

say() { printf '%s\n' "$*"; }
fail() { say "wiki-island could not be installed: $*"; exit 1; }

REPO_URL="https://github.com/takealook97/wiki-island"
ISSUES="$REPO_URL/issues"
BASE="${WI_INSTALL_BASE_URL:-$REPO_URL}"
BASE="${BASE%/}"
BIN_DIR="$HOME/.cargo/bin"
PROGRAMS="wi wiki-island"
# Downloads use https only; a test base URL may also use http.
PROTO="=https"
[ -z "${WI_INSTALL_BASE_URL:-}" ] || PROTO="=http,https"

OS=$(uname -s)
case "$OS" in
  Darwin | Linux) ;;
  *) fail "this installer supports macOS and Linux only." ;;
esac

TMP=$(mktemp -d) || fail "a temporary folder could not be created."
trap 'rm -rf "$TMP"' EXIT

# Prints the release target for this computer, or nothing when there is no prebuilt program for it.
release_target() {
  arch=$(uname -m)
  case "$OS/$arch" in
    Darwin/arm64 | Darwin/aarch64) echo "aarch64-apple-darwin" ;;
    Darwin/x86_64) echo "x86_64-apple-darwin" ;;
    Linux/aarch64 | Linux/arm64) echo "aarch64-unknown-linux-gnu" ;;
    Linux/x86_64) echo "x86_64-unknown-linux-gnu" ;;
    *) echo "" ;;
  esac
}

# Downloads $1 to $2. Sets FETCH to ok, missing (HTTP 404) or error (anything else).
fetch() {
  rc=0
  code=$(curl -sS -L --proto "$PROTO" --connect-timeout 20 --max-time 900 \
    -o "$2" -w '%{http_code}' "$1" 2>/dev/null) || rc=$?
  if [ "$rc" -ne 0 ]; then
    FETCH=error
  elif [ "$code" = "200" ]; then
    FETCH=ok
  elif [ "$code" = "404" ]; then
    FETCH=missing
  else
    FETCH=error
  fi
}

# Prints the SHA-256 of file $1.
sha256_of() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

# Sets tag from the redirect of releases/latest. Returns 1 (source build) when there is no
# published release, GitHub cannot be reached, or the tag has an unexpected form.
resolve_tag() {
  rc=0
  location=$(curl -sS --proto "$PROTO" --connect-timeout 20 --max-time 60 \
    -o /dev/null -w '%{http_code} %{redirect_url}' "$BASE/releases/latest" 2>/dev/null) || rc=$?
  if [ "$rc" -ne 0 ]; then
    say "GitHub could not be reached, so wiki-island will be built from source."
    return 1
  fi
  status=${location%% *}
  redirect=${location#* }
  case "$status/$redirect" in
    30[12378]/*/releases/tag/*) tag=${redirect##*/releases/tag/} ;;
    *)
      say "No prebuilt wiki-island is published yet, so it will be built from source."
      return 1
      ;;
  esac
  case "$tag" in
    v*[!A-Za-z0-9._-]* | v) ;;
    v*) return 0 ;;
  esac
  say "The latest release has an unexpected name, so wiki-island will be built from source."
  return 1
}

# Sets WANT to the digest for asset $1 from $TMP/SHA256SUMS. Exactly one line may name the asset
# (optionally with the binary-mode '*'); it must have two fields and a 64-digit hex digest.
check_manifest() {
  bad="the published checksum list is damaged, so nothing was installed. Run this installer again later, or report it at $ISSUES."
  lines=$(awk -v n="$1" '{ sub(/\r$/, ""); f = $2; sub(/^\*/, "", f); if (f == n) print NF, tolower($1) }' \
    "$TMP/SHA256SUMS") || fail "$bad"
  [ -n "$lines" ] || fail "$bad"
  [ "$(printf '%s\n' "$lines" | wc -l | tr -d ' ')" = "1" ] || fail "$bad"
  [ "${lines%% *}" = "2" ] || fail "$bad"
  WANT=${lines#* }
  case "$WANT" in
    *[!0-9a-f]* | "") fail "$bad" ;;
  esac
  [ "${#WANT}" -eq 64 ] || fail "$bad"
}

archive_unexpected() {
  fail "the downloaded release contains unexpected files, so nothing was installed. Report it at $ISSUES, then run this installer again later."
}

# Stops unless archive $1 holds exactly wi, wiki-island, LICENSE and README.md as regular files
# (each optionally prefixed by ./) and nothing else.
check_archive() {
  names=$(tar -tzf "$1" 2>/dev/null) || archive_unexpected
  types=$(tar -tvzf "$1" 2>/dev/null) || archive_unexpected
  printf '%s\n' "$types" | awk 'substr($0, 1, 1) != "-" { bad = 1 } END { exit bad }' \
    || archive_unexpected
  seen=""
  while IFS= read -r entry; do
    entry=${entry#./}
    case "$entry" in
      wi | wiki-island | LICENSE | README.md) ;;
      *) archive_unexpected ;;
    esac
    case " $seen " in
      *" $entry "*) archive_unexpected ;;
    esac
    seen="$seen $entry"
  done <<LIST
$names
LIST
  [ "$(printf '%s\n' "$names" | wc -l | tr -d ' ')" = "4" ] || archive_unexpected
  for want in wi wiki-island LICENSE README.md; do
    case " $seen " in
      *" $want "*) ;;
      *) archive_unexpected ;;
    esac
  done
}

# Tries the release install. Returns 0 when the programs are staged in $TMP/stage and 1 when the
# source build should run instead. A failed checksum stops the whole installer.
try_release() {
  [ "${WI_INSTALL_FROM_SOURCE:-}" != "1" ] || return 1
  target=$(release_target)
  if [ -z "$target" ]; then
    say "There is no prebuilt wiki-island for this computer, so it will be built from source."
    return 1
  fi
  if ! command -v curl >/dev/null 2>&1; then
    say "curl was not found, so wiki-island will be built from source; install curl to get the ready-made program next time."
    return 1
  fi
  if ! command -v shasum >/dev/null 2>&1 && ! command -v sha256sum >/dev/null 2>&1; then
    say "No checksum tool (shasum or sha256sum) was found, so wiki-island will be built from source."
    return 1
  fi

  resolve_tag || return 1

  asset="wiki-island-$tag-$target.tar.gz"
  say "Downloading wiki-island $tag..."
  fetch "$BASE/releases/download/$tag/$asset" "$TMP/$asset"
  case "$FETCH" in
    ok) ;;
    missing)
      say "This release has no program for this computer, so wiki-island will be built from source."
      return 1
      ;;
    *)
      say "The download did not finish, so wiki-island will be built from source."
      return 1
      ;;
  esac
  fetch "$BASE/releases/download/$tag/SHA256SUMS" "$TMP/SHA256SUMS"
  case "$FETCH" in
    ok) ;;
    missing)
      say "This release has no checksum list, so wiki-island will be built from source."
      return 1
      ;;
    *)
      say "The checksum list did not download, so wiki-island will be built from source."
      return 1
      ;;
  esac

  check_manifest "$asset"
  have=$(sha256_of "$TMP/$asset")
  [ "$have" = "$WANT" ] \
    || fail "the downloaded program does not match its checksum, so nothing was installed. Run this installer again later, or report it at $ISSUES."

  check_archive "$TMP/$asset"
  mkdir -p "$TMP/stage"
  tar -xzf "$TMP/$asset" -C "$TMP/stage" 2>/dev/null \
    || fail "the downloaded program could not be unpacked, so nothing was installed. Run this installer again later, or report it at $ISSUES."
  for name in $PROGRAMS; do
    if [ ! -f "$TMP/stage/$name" ] || [ -L "$TMP/stage/$name" ]; then
      archive_unexpected
    fi
  done
  chmod 755 "$TMP/stage/wi" "$TMP/stage/wiki-island"
  for name in $PROGRAMS; do
    if ! "$TMP/stage/$name" --version >/dev/null 2>&1; then
      say "The downloaded program does not run on this computer, so wiki-island will be built from source."
      return 1
    fi
  done
  return 0
}

# Puts back what was there before a failed replacement and removes staged copies.
rollback() {
  for name in $PROGRAMS; do
    if [ -e "$BIN_DIR/.$name.old.$$" ] || [ -L "$BIN_DIR/.$name.old.$$" ]; then
      mv -f "$BIN_DIR/.$name.old.$$" "$BIN_DIR/$name" 2>/dev/null || true
    elif [ -e "$TMP/placed.$name" ]; then
      rm -f "$BIN_DIR/$name"
    fi
    rm -f "$BIN_DIR/.$name.new.$$"
  done
}

# Replaces both installed programs with the staged ones, or neither.
install_staged() {
  keep="Check that your home folder has free space and can be written to, then run this installer again."
  mkdir -p "$BIN_DIR" || fail "the program folder could not be created. $keep"
  for name in $PROGRAMS; do
    if [ -e "$BIN_DIR/$name" ] && [ ! -f "$BIN_DIR/$name" ]; then
      fail "a folder named $name is in the way in ~/.cargo/bin. Move it aside, then run this installer again."
    fi
  done
  for name in $PROGRAMS; do
    if ! cp "$TMP/stage/$name" "$BIN_DIR/.$name.new.$$" || ! chmod 755 "$BIN_DIR/.$name.new.$$"; then
      rollback
      fail "the new programs could not be copied into place, and the previous ones were kept. $keep"
    fi
  done
  for name in $PROGRAMS; do
    if [ -e "$BIN_DIR/$name" ] || [ -L "$BIN_DIR/$name" ]; then
      if ! mv -f "$BIN_DIR/$name" "$BIN_DIR/.$name.old.$$"; then
        rollback
        fail "the previous programs could not be moved aside, and they were kept. $keep"
      fi
    fi
  done
  for name in $PROGRAMS; do
    if ! mv -f "$BIN_DIR/.$name.new.$$" "$BIN_DIR/$name"; then
      rollback
      fail "the new programs could not be put in place, and the previous ones were kept. $keep"
    fi
    : > "$TMP/placed.$name"
  done
  for name in $PROGRAMS; do
    rm -f "$BIN_DIR/.$name.old.$$"
  done
}

# Builds and installs from source.
build_from_source() {
  if [ -n "${WI_INSTALL_SOURCE_CMD:-}" ]; then
    sh -c "$WI_INSTALL_SOURCE_CMD" || fail "the build did not finish."
    return 0
  fi

  # 1. Tools for building: Apple's command line tools on macOS, a C compiler on Linux, and git.
  if [ "$OS" = "Darwin" ]; then
    if ! xcode-select -p >/dev/null 2>&1 || ! command -v git >/dev/null 2>&1; then
      say "Your Mac needs Apple's command line tools first (they include git)."
      say "Run this one command, wait for it to finish, then run this installer again:"
      say ""
      say "    xcode-select --install"
      exit 1
    fi
  else
    command -v cc >/dev/null 2>&1 \
      || fail "a C compiler was not found. Install one (for example the build-essential or gcc package), then run this installer again."
    command -v git >/dev/null 2>&1 \
      || fail "git was not found. Install git, then run this installer again."
  fi

  # 2. Rust. The repository picks the right version by itself.
  if ! command -v cargo >/dev/null 2>&1; then
    if [ -x "$HOME/.cargo/bin/cargo" ]; then
      PATH="$HOME/.cargo/bin:$PATH"; export PATH
    else
      say "Installing the Rust build tool (one time, needs the internet)..."
      command -v curl >/dev/null 2>&1 \
        || fail "curl was not found. Install curl (for example the curl package), then run this installer again."
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --default-toolchain none \
        || fail "the Rust installer did not finish. Check your internet connection and try again."
      PATH="$HOME/.cargo/bin:$PATH"; export PATH
    fi
  fi

  # 3. Find the source: this checkout, or a fresh clone when the script was piped into sh.
  if [ -f "$0" ]; then
    ROOT=$(cd "$(dirname "$0")/.." && pwd)
    [ -f "$ROOT/crates/wi-cli/Cargo.toml" ] || fail "run this script from a full copy of the repository."
  else
    say "Downloading the wiki-island source..."
    git clone --depth 1 "$BASE.git" "$TMP/src" >/dev/null 2>&1 \
      || fail "the source could not be downloaded. Check your internet connection and try again."
    ROOT="$TMP/src"
  fi

  # 4. Build and install.
  say "Building wiki-island. The first build takes a few minutes..."
  (cd "$ROOT" && cargo install --path crates/wi-cli --locked) \
    || fail "the build did not finish. Run the installer again; it continues where it can."
}

# Adds the PATH line to $1 once.
add_path_line() {
  if ! grep -qsF "$PATH_LINE" "$1"; then
    printf '\n%s\n' "$PATH_LINE" >> "$1" || fail "could not update $1."
  fi
  PROFILES="${PROFILES:+$PROFILES, }$1"
}

if try_release; then
  install_staged
else
  build_from_source
fi

# Make `wi` findable in new Terminal windows (added once to the shell startup files).
# shellcheck disable=SC2016  # the line is written literally into the profile
PATH_LINE='export PATH="$HOME/.cargo/bin:$PATH"'
PROFILES=""
if [ "$OS" = "Darwin" ]; then
  case "${SHELL:-}" in
    */bash) add_path_line "$HOME/.bash_profile" ;;
    *) add_path_line "$HOME/.zprofile" ;;
  esac
else
  case "${SHELL:-}" in
    */bash)
      add_path_line "$HOME/.bashrc"
      add_path_line "$HOME/.profile"
      ;;
    */zsh) add_path_line "$HOME/.zshrc" ;;
    *) add_path_line "$HOME/.profile" ;;
  esac
fi

# Connect your notes folder and AI apps (full path, so this works right now).
WI="$BIN_DIR/wi"
[ -x "$WI" ] || WI=$(command -v wi || true)
[ -n "$WI" ] || fail "the wi program was not found after the install."
"$WI" setup

say ""
say "Done. Nothing else to do: talk to your AI as usual."
say "New Terminal windows will find the wi command (it was added to $PROFILES)."
