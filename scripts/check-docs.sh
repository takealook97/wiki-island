#!/bin/sh
# Checks the documentation: relative Markdown links resolve, `#anchor` fragments match a heading
# in the target file, and the tests that docs/VALIDATION.md names exist.
#
# Links. Files checked: README.md, CONTRIBUTING.md, AGENTS.md and docs/**/*.md. Links in fenced
# code blocks (tracked by marker character and length) and inline code spans are ignored, as are links with a URL scheme (http:, mailto:).
# A link target must exist as a file or directory, relative to the linking file (a leading `/`
# means the repository root). An anchor is checked only against Markdown targets, using GitHub's
# heading slug rule: lowercase, keep letters, digits, spaces, hyphens and underscores, turn
# spaces into hyphens, and suffix repeated slugs with -1, -2, ... ATX and Setext headings count.
#
# VALIDATION citation format. docs/VALIDATION.md cites tests inside backticks on the same line:
#   - A file path: `crates/wi-store/tests/query_cap.rs`, or the same without the leading
#     `crates/` (`wi-store/tests/sync.rs`). A backticked path that starts with crates/, scripts/,
#     docs/, .github/ or a crate directory (wi-*, wiki-island) and ends in .rs, .sh, .ps1, .py,
#     .yml, .yaml, .toml or .md must exist, as written or under `crates/`.
#   - A test function name: a lowercase snake_case identifier with at least three underscores
#     (`an_over_cap_search_is_an_invalid_request_with_a_fixed_message`). It must appear as
#     `fn <name>` in a .rs file cited on the same line, or anywhere under crates/ when the line
#     cites no .rs file.
#   - A name prefix for a group of tests: a snake_case prefix ending in `_` or `_*` with at least
#     two underscores (`require_readable_*`). Some `fn <prefix>...` must exist, found the same way.
# Function citations with fewer than three underscores are not checked, by design.
# Other backticked text (commands, environment variables, short identifiers) is not checked.
#
# Run from anywhere: sh scripts/check-docs.sh
# Prints one `file:line: message` per failure and exits 1; prints nothing and exits 0 otherwise.
set -eu

cd "${WI_DOCS_ROOT:-$(dirname "$0")/..}"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/wi-check-docs.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
failures="$tmp/failures"
: >"$failures"

fail() {
  printf '%s:%s: %s\n' "$1" "$2" "$3" >>"$failures"
}

# Markdown files to check, one per line.
list_docs() {
  for f in README.md CONTRIBUTING.md AGENTS.md; do
    [ -f "$f" ] && printf '%s\n' "$f"
  done
  if [ -d docs ]; then
    find docs -type f -name '*.md' | sort
  fi
}

# Shared awk function: fence_skip() is true for a line that opens, closes or sits inside a fenced
# code block. An opening fence is up to 3 spaces then 3+ backticks or 3+ tildes; the block closes
# only on the same character repeated at least as many times, followed by nothing but spaces.
# shellcheck disable=SC2016
FENCE_AWK='
  function fence_skip(   t, m) {
    t = $0
    if (fchar == "") {
      if (match(t, /^ ? ? ?(```+|~~~+)/)) {
        m = substr(t, RSTART, RLENGTH)
        sub(/^ +/, "", m)
        fchar = substr(m, 1, 1)
        flen = length(m)
        return 1
      }
      return 0
    }
    if (match(t, /^ ? ? ?(```+|~~~+)[ \t]*$/)) {
      m = t
      gsub(/[ \t]/, "", m)
      if (substr(m, 1, 1) == fchar && length(m) >= flen) { fchar = ""; flen = 0 }
    }
    return 1
  }
'

# Plain awk only (no regex intervals, no gawk extensions): macOS awk and Ubuntu mawk must agree.
# Prints "line<TAB>target" for every link in a Markdown file.
extract_links() {
  awk "$FENCE_AWK"'
    fence_skip() { next }
    {
      line = $0
      # Drop inline code spans so that examples are not treated as links.
      while ((s = index(line, "`")) > 0) {
        rest = substr(line, s + 1)
        e = index(rest, "`")
        if (e == 0) break
        line = substr(line, 1, s - 1) " " substr(rest, e + 1)
      }
      work = line
      while (match(work, /\]\([^)]*\)/)) {
        target = substr(work, RSTART + 2, RLENGTH - 3)
        work = substr(work, RSTART + RLENGTH)
        sub(/^[ \t]+/, "", target)
        sub(/[ \t].*$/, "", target)
        gsub(/^<|>$/, "", target)
        if (target != "") printf "%d\t%s\n", NR, target
      }
      if (match(line, /^ ? ? ?\[[^]]+\]:[ \t]+[^ \t]+/)) {
        def = substr(line, RSTART, RLENGTH)
        sub(/^[^]]*\]:[ \t]+/, "", def)
        gsub(/^<|>$/, "", def)
        printf "%d\t%s\n", NR, def
      }
    }
  ' "$1"
}

# Prints the heading slugs of a Markdown file, one per line, with GitHub duplicate suffixes.
heading_slugs() {
  awk "$FENCE_AWK"'
    function emit(text) {
      # A link renders as its text.
      while (match(text, /\[[^]]*\]\([^)]*\)/)) {
        piece = substr(text, RSTART, RLENGTH)
        sub(/\]\(.*$/, "", piece)
        sub(/^\[/, "", piece)
        text = substr(text, 1, RSTART - 1) piece substr(text, RSTART + RLENGTH)
      }
      text = tolower(text)
      gsub(/[^a-z0-9 _-]/, "", text)
      gsub(/ /, "-", text)
      if (text in seen) {
        seen[text]++
        print text "-" (seen[text] - 1)
      } else {
        seen[text] = 1
        print text
      }
    }
    fence_skip() { prev = ""; next }
    /^ ? ? ?#+[ \t]/ {
      prev = ""
      text = $0
      sub(/^ ? ? ?/, "", text)
      hashes = text
      sub(/[ \t].*$/, "", hashes)
      if (length(hashes) > 6) next
      sub(/^#+[ \t]+/, "", text)
      sub(/[ \t]+#+[ \t]*$/, "", text)
      emit(text)
      next
    }
    prev != "" && /^ ? ? ?(=+|-+)[ \t]*$/ {
      emit(prev)
      prev = ""
      next
    }
    /^[ \t]*$/ { prev = ""; next }
    /^[ \t]*([-*+]|[0-9]+[.)])([ \t]|$)/ { prev = ""; next }
    {
      prev = $0
      sub(/^[ \t]+/, "", prev)
      sub(/[ \t]+$/, "", prev)
    }
  ' "$1"
}

has_slug() {
  heading_slugs "$1" | grep -Fxq -- "$2"
}

check_links() {
  list_docs | while IFS= read -r file; do
    dir=$(dirname "$file")
    extract_links "$file" | while IFS="$(printf '\t')" read -r line target; do
      case "$target" in
        [A-Za-z]*:*) continue ;;
      esac
      path=${target%%#*}
      anchor=
      case "$target" in
        *'#'*) anchor=${target#*#} ;;
      esac
      if [ -z "$path" ]; then
        resolved=$file
      else
        case "$path" in
          /*) resolved=.$path ;;
          *) resolved=$dir/$path ;;
        esac
        if [ ! -e "$resolved" ]; then
          fail "$file" "$line" "broken link: $path does not exist"
          continue
        fi
      fi
      if [ -n "$anchor" ] && [ -f "$resolved" ]; then
        case "$resolved" in
          *.md)
            lower=$(printf '%s' "$anchor" | tr '[:upper:]' '[:lower:]')
            if ! has_slug "$resolved" "$lower"; then
              fail "$file" "$line" "broken anchor: no heading #$anchor in ${resolved#./}"
            fi
            ;;
        esac
      fi
    done
  done
}

# Prints "line<TAB>kind<TAB>token" for every checkable backticked token in VALIDATION.md.
# kind is file, name or prefix; a line's file tokens are printed before its name tokens.
extract_citations() {
  awk "$FENCE_AWK"'
    fence_skip() { next }
    {
      rest = $0
      nf = 0; nn = 0
      while ((s = index(rest, "`")) > 0) {
        rest = substr(rest, s + 1)
        e = index(rest, "`")
        if (e == 0) break
        tok = substr(rest, 1, e - 1)
        rest = substr(rest, e + 1)
        if (tok ~ /^(crates|scripts|docs|\.github|wi-[a-z0-9-]+|wiki-island)\/[A-Za-z0-9_.\/-]+\.(rs|sh|ps1|py|yml|yaml|toml|md)$/) {
          files[++nf] = tok
        } else if (tok ~ /^[a-z][a-z0-9_]*(_\*|_)$/ && gsub(/_/, "_", tok) >= 2) {
          names[++nn] = "prefix\t" tok
        } else if (tok ~ /^[a-z][a-z0-9_]*$/ && gsub(/_/, "_", tok) >= 3) {
          names[++nn] = "name\t" tok
        }
      }
      for (i = 1; i <= nf; i++) printf "%d\tfile\t%s\n", NR, files[i]
      for (i = 1; i <= nn; i++) printf "%d\t%s\n", NR, names[i]
    }
  ' "$1"
}

# Prints the existing path for a cited file, as written or under crates/.
resolve_cited() {
  if [ -f "$1" ]; then
    printf '%s\n' "$1"
  elif [ -f "crates/$1" ]; then
    printf '%s\n' "crates/$1"
  else
    return 1
  fi
}

fn_pattern() {
  case "$1" in
    *'_*') printf 'fn[[:space:]]+%s[A-Za-z0-9_]*' "${1%\*}" ;;
    *_) printf 'fn[[:space:]]+%s[A-Za-z0-9_]*' "$1" ;;
    *) printf 'fn[[:space:]]+%s([^A-Za-z0-9_]|$)' "$1" ;;
  esac
}

check_validation() {
  validation=docs/VALIDATION.md
  [ -f "$validation" ] || return 0
  cited="$tmp/cited"
  : >"$cited"
  current=
  extract_citations "$validation" >"$tmp/citations"
  while IFS="$(printf '\t')" read -r line kind token; do
    if [ "$line" != "$current" ]; then
      current=$line
      : >"$cited"
    fi
    case "$kind" in
      file)
        if found=$(resolve_cited "$token"); then
          case "$found" in
            *.rs) printf '%s\n' "$found" >>"$cited" ;;
          esac
        else
          fail "$validation" "$line" "cited file $token does not exist"
        fi
        ;;
      name | prefix)
        pattern=$(fn_pattern "$token")
        if [ -s "$cited" ]; then
          # shellcheck disable=SC2046
          if ! grep -Eq -- "$pattern" $(cat "$cited"); then
            fail "$validation" "$line" "test $token not found in the cited file(s)"
          fi
        elif ! grep -rEq --include='*.rs' -- "$pattern" crates; then
          fail "$validation" "$line" "test $token not found under crates/"
        fi
        ;;
    esac
  done <"$tmp/citations"
}

check_links
check_validation

if [ -s "$failures" ]; then
  sort -t: -k1,1 -k2,2n "$failures" | uniq
  exit 1
fi
