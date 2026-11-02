#!/bin/sh
# Fail if the tree contains secrets, private host names or addresses, personal data, or paths from
# a developer's machine. Runs on every change and over a tree before it is first imported.
#
#   scripts/scrub-check.sh            # check the files git tracks in this repository
#   scripts/scrub-check.sh DIR        # check every file under DIR (for an import tree)
#
# Three checks:
#   1. scripts/scrub-patterns.txt: generic patterns (emails, private IPv4 ranges, tailnet names,
#      absolute paths), minus scripts/scrub-allow.txt and lines marked "scrub:allow".
#   2. A private list of literals kept outside the repository, one per line, matched as fixed
#      strings case-insensitively: $STREAMCORE_SCRUB_DENYLIST, default
#      ~/.config/streamcore/scrub-denylist.txt. It fails closed when the list is missing, except on
#      hosted CI (GITHUB_ACTIONS=true), which never has it.
#   3. gitleaks, when installed, for credentials.
set -eu
cd "$(dirname "$0")/.."
ROOT=$(pwd)
PATTERNS="$ROOT/scripts/scrub-patterns.txt"
ALLOW="$ROOT/scripts/scrub-allow.txt"
DENYLIST=${STREAMCORE_SCRUB_DENYLIST:-$HOME/.config/streamcore/scrub-denylist.txt}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# The files to check (NUL-separated), minus the licence text and the scrub lists themselves.
if [ $# -ge 1 ]; then
  TARGET=$(cd "$1" && pwd)
  (cd "$TARGET" && find . -type f -not -path './.git/*' -print0) > "$TMP/files"
else
  TARGET=$ROOT
  git ls-files -z > "$TMP/files"
fi
tr '\0' '\n' < "$TMP/files" | sed 's|^\./||' |
  grep -v -x -e LICENSE -e scripts/scrub-patterns.txt -e scripts/scrub-allow.txt > "$TMP/list" || true
cd "$TARGET"
FAIL=0

grep -v -e '^#' -e '^$' "$PATTERNS" > "$TMP/patterns"
grep -v -e '^#' -e '^$' "$ALLOW" > "$TMP/allow"
if [ -s "$TMP/list" ]; then
  tr '\n' '\0' < "$TMP/list" | xargs -0 grep -I -n -H -i -E -f "$TMP/patterns" -- 2>/dev/null |
    grep -v -F 'scrub:allow' > "$TMP/hits1" || true
  # Drop hits whose every match is allowed: remove allowed strings, then re-test the line.
  while IFS= read -r hit; do
    rest=$(printf '%s\n' "$hit" | cut -d: -f3-)
    while IFS= read -r a; do rest=$(printf '%s' "$rest" | awk -v a="$a" 'BEGIN{IGNORECASE=1} {n=index(tolower($0),tolower(a)); while(n>0){$0=substr($0,1,n-1) substr($0,n+length(a)); n=index(tolower($0),tolower(a))} print}'); done < "$TMP/allow"
    if printf '%s\n' "$rest" | grep -q -i -E -f "$TMP/patterns"; then printf '%s\n' "$hit"; fi
  done < "$TMP/hits1" > "$TMP/hits"
  if [ -s "$TMP/hits" ]; then
    echo "scrub: generic patterns matched (add 'scrub:allow' with a reason only if the match is public by design):" >&2
    cat "$TMP/hits" >&2
    FAIL=1
  fi
fi

if [ -f "$DENYLIST" ]; then
  grep -v -e '^#' -e '^$' "$DENYLIST" > "$TMP/deny" || true
  if [ -s "$TMP/deny" ] && [ -s "$TMP/list" ]; then
    if tr '\n' '\0' < "$TMP/list" | xargs -0 grep -I -l -i -F -f "$TMP/deny" -- 2>/dev/null > "$TMP/denyhits"; then
      echo "scrub: private denylist entries found in (contents not shown):" >&2
      cat "$TMP/denyhits" >&2
      FAIL=1
    fi
  fi
elif [ "${GITHUB_ACTIONS:-}" != true ]; then
  echo "scrub: the private denylist $DENYLIST is missing; refusing to pass without it." >&2
  FAIL=1
fi

if command -v gitleaks > /dev/null 2>&1; then
  # In a repository, scan only the tracked files: build output (target/) is not checked in
  # and contains key-shaped test vectors from dependencies.
  LEAKS_SOURCE=$TARGET
  if [ $# -eq 0 ]; then
    LEAKS_SOURCE="$TMP/tracked"
    git -C "$ROOT" checkout-index -a --prefix="$LEAKS_SOURCE/"
  fi
  if ! gitleaks detect --no-git --no-banner --redact --source "$LEAKS_SOURCE" > "$TMP/gitleaks" 2>&1; then
    echo "scrub: gitleaks found possible credentials:" >&2
    cat "$TMP/gitleaks" >&2
    FAIL=1
  fi
fi

[ "$FAIL" = 0 ] && echo "scrub: clean ($(wc -l < "$TMP/list" | tr -d ' ') files)"
exit "$FAIL"
