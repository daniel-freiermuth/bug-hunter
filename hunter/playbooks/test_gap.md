You are hunting for TEST COVERAGE GAPS in the repository at {{REPO_PATH}} ({{REPO_NAME}}).
Read-only investigation: do NOT modify the repo, do NOT run formatters or full
test suites. Output is candidate test gaps only.

# Repository Context
{{REPO_NOTES}}

# Scope
{{SCOPE_NOTE}}

Focus on functions and methods with complex behavior lacking automated test coverage.
Weight recent code heavily — fresh features are where gaps matter most.

# What counts as a test gap
Functions/methods with non-trivial contracts that lack tests for:
- **Error paths**: invalid inputs, boundary violations, edge cases
- **Boundary conditions**: empty collections, first/last elements, wraparound, limits
- **State transitions**: lifecycle methods, cleanup, initialization
- **Async flows**: race conditions, cancellation, timeout handling
- **Integration points**: API contracts, cross-module dependencies

NOT gaps: trivial getters/setters, pure plumbing (pass-through), thoroughly tested code, internal utilities already covered indirectly.

# When the untested code is already wrong
If verifying a gap shows the CURRENT code misbehaves — wrong output, a
crash, a misparse, data dropped, an input it should refuse let through —
that is a bug, not a missing test. File it as a bug instead (same output
file, `"type": "bug"`, bug shape below) and do NOT also file the gap: the
bug fix brings its own regression test. File a test gap only for behavior
that is correct today and merely unguarded. "Could regress" is a gap; "does
the wrong thing" is a bug.

```json
{
  "type": "bug",
  "fingerprint": "{{REPO_NAME}}:path/file.ext:symbol:bug-class",
  "file": "path/file.ext",
  "symbol": "functionOrMethod",
  "line": 0,
  "bug_class": "boundary|error-path|race|contract-drift|leak|logic",
  "severity": "high|medium|low",
  "confidence": 0.0,
  "summary": "one sentence: what goes wrong",
  "detail": "what breaks, why, with file:line code evidence",
  "evidence_plan": "how to PROVE it + reachable rung: 1=failing automated test, 2=scripted repro, 3=argued trace"
}
```

# Known non-candidates (suppression corpus — do NOT re-file these or variants)
Rejected gaps and rejected bugs alike.
{{SUPPRESSIONS}}

# Already tracked (open gaps and bugs — file only if yours is genuinely NOVEL)
{{KNOWN_GAPS}}

# Output contract — INCREMENTAL, you may be killed at any moment
Create {{OUT_PATH}} containing `[]` as your VERY FIRST action. After EACH
verified gap or bug, rewrite the complete file with everything confirmed so
far — committed entries survive a kill, anything only in your head does not.
Max {{MAX_GAPS}} entries, gaps and bugs together. Each test gap entry:

```json
{
  "fingerprint": "{{REPO_NAME}}:path/file.ext:symbol:test-gap",
  "file": "path/file.ext",
  "symbol": "functionOrMethod",
  "line": 0,
  "severity": "high|medium|low",
  "confidence": 0.0,
  "summary": "one sentence describing the gap",
  "missing_tests": ["error path: invalid input", "boundary: empty array"],
  "detail": "why these tests matter, with file:line evidence",
  "test_file": "path/to/test/file.ext"
}
```

Every gap MUST be verified against current code with file:line evidence.
Focus on HIGH-VALUE gaps — tests that would catch real bugs. Then stop.
