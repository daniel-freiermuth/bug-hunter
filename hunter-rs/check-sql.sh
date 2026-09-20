#!/usr/bin/env bash
# The SQL encapsulation guarantees, in one place: `just lint` and the CI
# "SQL encapsulation" step both run this script. They used to spell the same
# three greps out twice in different shell dialects, which only ever agreed
# by hand.
#
# Every check is a grep pipeline, and grep's exit codes carry three meanings:
# 0 matched, 1 nothing matched, anything above that grep itself failed
# (unreadable path, invalid regex). A bare `if pipeline; then` collapses the
# last two into "clean", so a check that never ran reads as a check that
# passed. `pipefail` does not rescue that: it yields the *last* nonzero
# status, so a downstream no-match (1) overwrites an upstream failure (2).
# Only `${PIPESTATUS[@]}`, captured on the line after the pipeline, can tell
# the three apart, which is what `matched` below does. pipefail stays on for
# the benefit of any future pipeline whose failure is an ordinary failure.
set -uo pipefail

cd "$(dirname "$0")" || exit 1

# The comment filter must be anchored: an unanchored `//` drops any line
# that merely *contains* a comment marker, so a real violation with a
# trailing comment (or a URL in a string literal) escapes the gate. Match
# `//` only where the code starts, i.e. right after grep's `path:line:`
# prefix, allowing indentation.
comment_line='^[^:]*:[0-9]+:[[:space:]]*//'

# Classify one pipeline from its per-stage statuses. Returns success when the
# pipeline matched, i.e. when there is a violation to report; aborts the whole
# script if any stage failed outright, because a failed stage means the
# guarantee was not tested at all.
matched() {
  local name=$1 st last=1
  shift
  for st in "$@"; do
    if ((st > 1)); then
      echo "✗ $name: grep exited $st, so this check did not run" >&2
      exit 2
    fi
    last=$st
  done
  ((last == 0))
}

echo "Checking pool() encapsulation..."
grep -rn '\.pool()' src/ | grep -v 'store\.rs' | grep -Ev "$comment_line"
status=("${PIPESTATUS[@]}")
if matched "pool() encapsulation" "${status[@]}"; then
  echo "✗ pool() leaked outside store.rs"
  exit 1
fi
echo "✓ pool() only in store.rs"

echo "Checking compile-time checked queries..."
grep -rn 'sqlx::query(' src/ | grep -v 'query!\|query_as!\|query_scalar!\|capacity\.rs' | grep -Ev "$comment_line"
status=("${PIPESTATUS[@]}")
if matched "compile-time checked queries" "${status[@]}"; then
  echo "✗ unchecked sqlx::query found"
  exit 1
fi
echo "✓ all queries compile-time checked"

echo "Checking no QueryBuilder..."
grep -rn 'QueryBuilder' src/ | grep -Ev "$comment_line"
status=("${PIPESTATUS[@]}")
if matched "no QueryBuilder" "${status[@]}"; then
  echo "✗ QueryBuilder found"
  exit 1
fi
echo "✓ no QueryBuilder"

echo "All SQL guarantees hold."
