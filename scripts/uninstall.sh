#!/bin/sh
# Removes what wiki-island added to this Mac. Your notes are kept.
set -eu

WI=$(command -v wi || true)
[ -n "$WI" ] || WI="$HOME/.cargo/bin/wi"
[ -x "$WI" ] || { printf '%s\n' "wiki-island is not installed (no wi program found)."; exit 0; }

"$WI" uninstall
printf '%s\n' ""
printf '%s\n' "Your notes were kept; delete the notes folder yourself if you no longer want them."
printf '%s\n' "To remove the wi program too, run: cargo uninstall wiki-island"
printf '%s\n' "To remove the Rust build tool as well (optional), run: rustup self uninstall"
