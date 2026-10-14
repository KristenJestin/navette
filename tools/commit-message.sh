#!/usr/bin/env bash
# Checks the Angular commit convention (see AGENTS.md, Git and pull requests).
#   tools/commit-message.sh <file>             one message (the commit-msg hook, a PR title)
#   tools/commit-message.sh --range <a..b|sha> every commit of a range
#   tools/commit-message.sh --pull-request <n> <file>
#                                              a PR title, judged as its squash subject: " (#n)" appended
set -euo pipefail

TYPES='feat|fix|refactor|test|docs|chore|build|ci|perf'
PATTERN="^($TYPES)\([a-z0-9-]+\)!?: [^A-Z].{0,}[^.]$"

check() { # <subject> <where> [<squash suffix>]
  local subject=$1 where=$2 suffix=${3:-}
  case "$subject" in Merge\ *|Revert\ *|fixup!*|squash!*) return 0 ;; esac
  if ! [[ $subject =~ $PATTERN ]]; then
    echo "$where: \"$subject\" is not <type>(<scope>): <subject> (types: ${TYPES//|/, }; scope required; lowercase start; no final period)" >&2
    return 1
  fi
  local merged=$subject$suffix
  if [ "${#merged}" -gt 72 ]; then
    echo "$where: subject is ${#merged} characters${suffix:+ once merged as \"$merged\"}, 72 at most" >&2
    return 1
  fi
}

if [ "${1:-}" = "--range" ]; then
  status=0
  while read -r sha; do
    check "$(git log -1 --format=%s "$sha")" "${sha:0:8}" || status=1
  done < <(git rev-list "$2")
  exit $status
fi

if [ "${1:-}" = "--pull-request" ]; then
  check "$(grep -v '^#' "$3" | head -1)" "${3##*/}" " (#$2)"
  exit
fi

check "$(grep -v '^#' "$1" | head -1)" "${1##*/}"
