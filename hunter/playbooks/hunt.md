You are hunting for LATENT BUGS in the repository at {{REPO_PATH}} ({{REPO_NAME}}).
Read-only investigation: do NOT modify the repo, do NOT run formatters or full
test suites. Output is candidate findings only.

# Scope
Diff-focused hunt over: `git diff {{DIFF_RANGE}}` and `git log {{DIFF_RANGE}}`.
{{SCOPE_NOTE}}
If the range contains no commits, write an empty findings array to the output
file and stop immediately — do not widen scope on your own.

For each suspicious hunk, read the CURRENT code around it — the full function,
its callers, related state — to confirm or kill the suspicion before filing.

# What counts as a bug
Only defects with observable wrong behavior: broken edge cases and boundary
mistakes (wrap-around, unit conversions, empty collections, first/last
element), error paths that drop data or swallow failures, contract drift
between components (API shape, field names, units, null handling), races and
stale state in async/event-driven code (captured indices/references across
async gaps, subscription lifecycles), resource leaks (listeners, timers,
unbounded growth), logic inversions. Style issues, missing tests, and
refactor opportunities are NOT bugs.

Fresh feature code is where bugs live — weight recently-introduced code
heavily. Bugs cluster: when you confirm one, inspect its siblings.

# Known non-bugs (suppression corpus — do NOT re-file these or variants)
{{SUPPRESSIONS}}

# Already tracked (open findings — file only if yours is genuinely NOVEL;
# refactors move code, so compare by mechanism, not location)
{{KNOWN_FINDINGS}}

# Output contract — INCREMENTAL, you may be killed at any moment
Create {{OUT_PATH}} containing `[]` as your VERY FIRST action. After EACH
verified finding, rewrite the complete file with everything confirmed so far
— committed findings survive a kill, anything only in your head does not.
Max {{MAX_FINDINGS}} entries; re-rank by severity × confidence when you
finish. Five credible findings beat twenty guesses. Each entry:

```json
{
  "fingerprint": "{{REPO_NAME}}:path/file.ext:symbol:bug-class",
  "file": "path/file.ext",
  "symbol": "functionOrMethod",
  "line": 0,
  "bug_class": "boundary|error-path|race|contract-drift|leak|logic",
  "severity": "high|medium|low",
  "confidence": 0.0,
  "summary": "one sentence",
  "detail": "what breaks, why, with file:line code evidence",
  "evidence_plan": "how to PROVE it + reachable rung: 1=failing automated test, 2=scripted repro, 3=argued trace",
  "introduced_by": "commit sha"
}
```

Every finding MUST be verified against current code with file:line evidence
and MUST state a concrete evidence plan. Then stop.
