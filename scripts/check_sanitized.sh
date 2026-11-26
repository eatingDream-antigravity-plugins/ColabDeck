#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
root=$(git rev-parse --show-toplevel 2>/dev/null) || exit 2
cd "$root" || exit 2
patterns=.github/sanitize-patterns.txt
temp=$(mktemp -d) || exit 2
trap 'rm -rf "$temp"' EXIT
rules="$temp/rules"
numbered_rules="$temp/numbered-rules"
hits="$temp/hits"
if ! awk -v rules="$rules" '!/^[[:space:]]*(#|$)/ {
  print > rules
  print NR ":" $0
}' "$patterns" > "$numbered_rules" || [[ ! -s "$rules" || ! -s "$numbered_rules" ]]; then
  echo "sanitization guard: cannot prepare rules" >&2
  exit 2
fi

if [[ $# -eq 2 && $1 == --stage ]]; then
  # Use the same rules on each staged text file and on printable binary strings.
  stage=$2
  [[ -f "$stage/bin/colab-mcp" ]] || exit 2
  cd "$stage" || exit 2
  find . -type f -print0 > "$temp/files" || exit 2
  failed=0
  scan_rule() {
    local mode=$1 rule=$2 pattern=$3 data=$4 file=$5 match status
    if grep -an "$mode" -e "$pattern" -- "$data" > "$hits"; then
      while IFS= read -r match; do
        printf '%q:%s rule %s\n' "$file" "${match%%:*}" "$rule"
      done < "$hits"
      failed=1
    else
      status=$?
      if [[ $status -ne 1 ]]; then exit 2; fi
    fi
  }
  while IFS= read -r -d '' file; do
    data=$file
    if [[ $file == ./bin/colab-mcp ]]; then
      data="$temp/strings"
      strings -a "$file" > "$data" || exit 2
    fi
    while IFS=: read -r line rule; do
      scan_rule -E "$line" "$rule" "$data" "$file"
    done < "$numbered_rules"
    if [[ -n ${HOME:-} && $HOME != / ]]; then
      scan_rule -F home "$HOME" "$data" "$file"
    fi
    scan_rule -F workspace "$root" "$data" "$file"
  done < "$temp/files"
  exit "$failed"
fi
[[ $# -eq 0 ]] || exit 2

# Keep matches private and print only the Git filename and line.
scan_git() {
  local scope=$1 status file line
  shift
  if git grep "$@" -z -nEI -f "$rules" -- . ":(exclude)$patterns" ":(exclude)scripts/check_sanitized.sh" > "$hits" 2>/dev/null; then
    while IFS= read -r -d '' file && IFS= read -r -d '' line && IFS= read -r _; do
      printf '%s %q:%s\n' "$scope" "$file" "$line"
    done < "$hits"
    failed=1
  else
    status=$?
    if [[ $status -ne 1 ]]; then
      echo "sanitization guard: $scope scanner failed" >&2
      exit 2
    fi
  fi
}
failed=0
scan_git worktree
scan_git index --cached
exit "$failed"
