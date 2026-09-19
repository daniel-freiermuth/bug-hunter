#!/usr/bin/env bash
# parity.sh — compare Python triage server vs Rust port, endpoint by endpoint.
#
# Usage: PY_PORT=8377 RS_PORT=8378 ./parity.sh
# Requires: curl, jq.
#
# NOTE: the live DB may change between the two fetches while the Python daemon
# is mid-cycle; on an unexpected FAIL, rerun once before investigating.
#
# (Main: chmod +x this file.)
#
# Deliberately no `set -e`: diff returns 1 on difference, which is expected.
set -uo pipefail

PY_PORT="${PY_PORT:-8377}"
RS_PORT="${RS_PORT:-8378}"
PY_BASE="http://127.0.0.1:${PY_PORT}"
RS_BASE="http://127.0.0.1:${RS_PORT}"

# Mismatched bodies go in a private scratch dir, not predictable /tmp paths
# (symlink/TOCTOU). Removed on exit; the inline diff is the durable record.
TMPDIR_PARITY="$(mktemp -d)" || exit 1
trap 'rm -rf -- "$TMPDIR_PARITY"' EXIT

fail_count=0
pass_count=0
skip_count=0

# slugify "/api/finding?id=3" -> "api-finding-id-3"
slug() {
    printf '%s' "$1" | sed -e 's/^\///' -e 's/[^A-Za-z0-9]\+/-/g' -e 's/-$//'
}

# compare <endpoint-path> [jq-filter]
# Fetches both servers, applies optional jq filter, normalizes with jq -S,
# diffs, and prints PASS/FAIL.
compare() {
    local path="$1"
    local filter="${2:-.}"
    local s py rs
    s="$(slug "$path")"

    if ! py="$(curl -fsS "${PY_BASE}${path}" | jq -S "$filter" 2>/dev/null)"; then
        echo "FAIL ${path} (python fetch/parse error)"
        fail_count=$((fail_count + 1))
        return
    fi
    if ! rs="$(curl -fsS "${RS_BASE}${path}" | jq -S "$filter" 2>/dev/null)"; then
        echo "FAIL ${path} (rust fetch/parse error)"
        fail_count=$((fail_count + 1))
        return
    fi

    if [ "$py" = "$rs" ]; then
        echo "PASS ${path}"
        pass_count=$((pass_count + 1))
    else
        echo "FAIL ${path}"
        fail_count=$((fail_count + 1))
        printf '%s\n' "$py" > "${TMPDIR_PARITY}/parity-${s}-py.json"
        printf '%s\n' "$rs" > "${TMPDIR_PARITY}/parity-${s}-rs.json"
        echo "  bodies: ${TMPDIR_PARITY}/parity-${s}-{py,rs}.json"
        diff -u "${TMPDIR_PARITY}/parity-${s}-py.json" "${TMPDIR_PARITY}/parity-${s}-rs.json" | head -n 40
    fi
}

skip() {
    echo "SKIP $1 ($2)"
    skip_count=$((skip_count + 1))
}

# Full-body comparisons.
compare /api/repos
compare /api/jobs
compare /api/events
compare /api/stats
compare /api/findings

# /api/summary: drop round-1 stub/volatile keys on BOTH sides; the stable
# comparable remainder is counts/type_counts/repos.
compare /api/summary 'del(.backend_status_html, .cycle_running, .next_candidate, .activity_status, .current_job, .last_cycle, .scheduler_state)'

# /api/finding?id=<N>: N from first finding on the Python side.
finding_id="$(curl -fsS "${PY_BASE}/api/findings" | jq -r '.[0].id // empty' 2>/dev/null)"
if [ -n "$finding_id" ]; then
    compare "/api/finding?id=${finding_id}"
else
    skip "/api/finding" "no findings"
fi

# /api/repo/notes?id=<N>: N from first repo id.
repo_id="$(curl -fsS "${PY_BASE}/api/repos" | jq -r '.[0].id // empty' 2>/dev/null)"
if [ -n "$repo_id" ]; then
    compare "/api/repo/notes?id=${repo_id}"
else
    skip "/api/repo/notes" "no repos"
fi

echo
echo "summary: ${pass_count} passed, ${fail_count} failed, ${skip_count} skipped"
if [ "$fail_count" -gt 0 ]; then
    exit 1
fi
exit 0
