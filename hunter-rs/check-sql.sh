#!/usr/bin/env bash
# The SQL encapsulation guarantees, in one place: `just lint` and the CI
# "SQL encapsulation" step both run this script. They used to spell the same
# three greps out twice in different shell dialects, which only ever agreed
# by hand.
#
# `pipefail` matters here: without it a grep that errors out (exit 2) is
# indistinguishable from a clean run, and the check passes having tested
# nothing.
set -uo pipefail

cd "$(dirname "$0")" || exit 1

# The comment filter must be anchored: an unanchored `//` drops any line
# that merely *contains* a comment marker, so a real violation with a
# trailing comment (or a URL in a string literal) escapes the gate. Match
# `//` only where the code starts, i.e. right after grep's `path:line:`
# prefix, allowing indentation.
comment_line='^[^:]*:[0-9]+:[[:space:]]*//'

echo "Checking pool() encapsulation..."
if grep -rn '\.pool()' src/ | grep -v 'store\.rs' | grep -Ev "$comment_line"; then
  echo "✗ pool() leaked outside store.rs"
  exit 1
fi
echo "✓ pool() only in store.rs"

echo "Checking compile-time checked queries..."
if grep -rn 'sqlx::query(' src/ | grep -v 'query!\|query_as!\|query_scalar!\|capacity\.rs' | grep -Ev "$comment_line"; then
  echo "✗ unchecked sqlx::query found"
  exit 1
fi
echo "✓ all queries compile-time checked"

echo "Checking no QueryBuilder..."
if grep -rn 'QueryBuilder' src/ | grep -Ev "$comment_line"; then
  echo "✗ QueryBuilder found"
  exit 1
fi
echo "✓ no QueryBuilder"

echo "All SQL guarantees hold."
