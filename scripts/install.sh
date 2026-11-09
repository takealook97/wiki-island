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
BASE="${WI_INSTALL_BASE_URL:-$REPO_URL}"
BASE="${BASE%/}"
BIN_DIR="$HOME/.cargo/bin"

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
  proto="=https"
  [ -z "${WI_INSTALL_BASE_URL:-}" ] || proto="=http,https"
  rc=0
  code=$(curl -sS -L --proto "$proto" --connect-timeout 20 --max-time 900 \
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
    say "curl was not found, so wiki-island will be built from source."
    return 1
  fi
  if ! command -v shasum >/dev/null 2>&1 && ! command -v sha256sum >/dev/null 2>&1; then
    say "No checksum tool (shasum or sha256sum) was found, so wiki-island will be built from source."
    return 1
  fi

  rc=0
  location=$(curl -sS --proto "=https,http" --connect-timeout 20 --max-time 60 \
    -o /dev/null -w '%{http_code} %{redirect_url}' "$BASE/releases/latest" 2>/dev/null) || rc=$?
  if [ "$rc" -ne 0 ]; then
    say "GitHub could not be reached, so wiki-island will be built from source."
    return 1
  fi
  status=${location%% *}
  tag=${location#* }
  tag=${tag##*/}
  case "$status" in
    301 | 302 | 303 | 307 | 308) ;;
    *)
      say "No wiki-island release was found, so it will be built from source."
      return 1
      ;;
  esac
  case "$tag" in
    "" | *[!A-Za-z0-9._-]*)
      say "The latest release has an unexpected name, so wiki-island will be built from source."
      return 1
      ;;
  esac

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

  matches=$(awk -v n="$asset" '$2 == n' "$TMP/SHA256SUMS" | wc -l | tr -d ' ')
  [ "$matches" = "1" ] || fail "the checksum list does not contain exactly one entry for $asset. Nothing was installed."
  want=$(awk -v n="$asset" '$2 == n { print tolower($1) }' "$TMP/SHA256SUMS")
  case "$want" in
    *[!0-9a-f]* | "") fail "the checksum list is malformed. Nothing was installed." ;;
  esac
  [ "${#want}" -eq 64 ] || fail "the checksum list is malformed. Nothing was installed."
  have=$(sha256_of "$TMP/$asset")
  [ "$have" = "$want" ] || fail "the downloaded file does not match its checksum. Nothing was installed."

  mkdir -p "$TMP/stage"
  if ! tar -xzf "$TMP/$asset" -C "$TMP/stage" 2>/dev/null \
    || [ ! -f "$TMP/stage/wi" ] || [ ! -f "$TMP/stage/wiki-island" ]; then
    say "The downloaded program could not be unpacked, so wiki-island will be built from source."
    return 1
  fi
  chmod 755 "$TMP/stage/wi" "$TMP/stage/wiki-island"
  if ! "$TMP/stage/wi" --version >/dev/null 2>&1; then
    say "The downloaded program does not run on this computer, so wiki-island will be built from source."
    return 1
  fi
  return 0
}

# Copies the staged programs into $BIN_DIR, replacing older ones in one step each.
install_staged() {
  mkdir -p "$BIN_DIR" || fail "could not create $BIN_DIR."
  for name in wi wiki-island; do
    cp "$TMP/stage/$name" "$BIN_DIR/.$name.new.$$" || fail "could not write to $BIN_DIR."
    chmod 755 "$BIN_DIR/.$name.new.$$"
    mv -f "$BIN_DIR/.$name.new.$$" "$BIN_DIR/$name" || fail "could not replace $BIN_DIR/$name."
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
      command -v curl >/dev/null 2>&1 || fail "curl was not found."
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
