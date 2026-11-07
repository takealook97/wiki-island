#!/bin/sh
# Wiki Island installer for macOS. Safe to run again. Needs no administrator rights.
set -eu

say() { printf '%s\n' "$*"; }
fail() { say "Wiki Island could not be installed: $*"; exit 1; }

[ "$(uname -s)" = "Darwin" ] || fail "this installer supports macOS only."

# 1. Tools that come with Apple's command line tools (git, compiler).
if ! xcode-select -p >/dev/null 2>&1 || ! command -v git >/dev/null 2>&1; then
  say "Your Mac needs Apple's command line tools first (they include git)."
  say "Run this one command, wait for it to finish, then run this installer again:"
  say ""
  say "    xcode-select --install"
  exit 1
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

# 3. Build and install `wi` from this repository.
ROOT=$(cd "$(dirname "$0")/.." && pwd)
[ -f "$ROOT/crates/wi-cli/Cargo.toml" ] || fail "run this script from a full copy of the repository."
say "Building Wiki Island. The first build takes a few minutes..."
cd "$ROOT"
cargo install --path crates/wi-cli --locked \
  || fail "the build did not finish. Run the installer again; it continues where it can."

# 4. Make `wi` findable in new Terminal windows (added once to the login profile).
case "${SHELL:-}" in
  */bash) PROFILE="$HOME/.bash_profile" ;;
  *) PROFILE="$HOME/.zprofile" ;;
esac
PATH_LINE='export PATH="$HOME/.cargo/bin:$PATH"'
if ! grep -qsF "$PATH_LINE" "$PROFILE"; then
  printf '\n%s\n' "$PATH_LINE" >> "$PROFILE" || fail "could not update $PROFILE."
fi

# 5. Connect your notes folder and AI apps (full path, so this works right now).
WI="$HOME/.cargo/bin/wi"
[ -x "$WI" ] || WI=$(command -v wi || true)
[ -n "$WI" ] || fail "the wi program was not found after the build."
"$WI" setup

say ""
say "Done. Nothing else to do: talk to your AI as usual."
say "New Terminal windows will find the wi command (it was added to $PROFILE)."
