PR #{{PR_NUMBER}} for repo {{REPO_NAME}} has already MERGED. You are reviewing
its complete lifetime — what it set out to do, everything discussed along the
way, and what actually shipped — to identify genuine follow-up work.
Read-only: do NOT modify the repo, do NOT commit anything, do NOT run
formatters or full test suites. The worktree at {{WORKTREE}} is checked out
at {{DEFAULT_BRANCH}}'s current HEAD, which already includes this PR's
changes.

# Repository Context
{{REPO_NOTES}}

# The finding this PR was for
```json
{{FINDING_JSON}}
```

# The PR
Title: {{PR_TITLE}}

{{PR_BODY}}

# Full discussion (chronological, newest last)
{{FEEDBACK}}

# Why this review happens now, not when the PR opened
A snapshot taken when a PR opens is the worst possible moment to judge what
follow-up work remains: nothing has been discussed yet. Over a PR's life,
scope can grow (a reviewer pushes for more, a blocking constraint turns out
to be liftable, so the PR absorbs the full goal itself) or shrink (something
else lands first and closes the gap). Only once merged is the true, final
shape of what shipped actually known. Your job: reconcile everything above
against what {{DEFAULT_BRANCH}} ACTUALLY ships now, and propose only what
genuinely remains open.

# Protocol
1. Read the diff that actually landed (`git -C {{WORKTREE}} log --oneline -5`
   / `git show`), not just the PR body or title — bodies go stale, written
   once and never updated as a PR's real scope grows through review.
2. Gather every candidate follow-up from three sources:
   - The PR body's own "what was deliberately NOT changed"-style section,
     if any
   - The discussion thread above — anything a human or reviewer flagged as
     "separate PR", "separate refactor", a still-open question, or an
     explicitly deferred concern
   - What you can see directly in the CURRENT code (e.g. a version still
     short of a stated target, a deprecation warning still present,
     something the PR body claimed but the diff doesn't actually show)
3. VERIFY each candidate against the current code before proposing it. This
   is the failure mode this whole mechanism exists to prevent: something
   discussed mid-PR may already have been resolved by a LATER commit in the
   same PR (real example: a dep-update PR was capped at an intermediate
   version when it opened, then a reviewer asked for more through ordinary
   comments, and the final commit reached the full original target —
   filing a follow-up for that gap would be filing a follow-up for
   something that no longer exists).
4. Only file HIGH-VALUE, concrete, actionable items with a clear next step.
   Not speculative "could also look at X" musings. Empty output is the
   correct, common outcome when a PR cleanly closed its own scope — do NOT
   invent something to justify having run.

# Deliverables
- FOLLOW-UPS.json at the worktree root, NOT COMMITTED, OPTIONAL — only if
  step 3 verified real, still-open work. A JSON array; the scheduler ingests
  it into the finding queue, so each item becomes a normal, triage-able
  finding instead of prose buried in a merged PR nobody re-reads. Empty
  array or omit the file entirely otherwise. Each entry MUST set its own
  "type" (bug | dep_update | test_gap | refactor | modernization) to
  whichever shape actually fits, e.g. for a leftover cleanup item:
  ```json
  {
    "type": "refactor",
    "fingerprint": "{{REPO_NAME}}:path/file.ext:short-slug",
    "file": "path/file.ext",
    "smell_type": "outdated-pattern",
    "severity": "low",
    "confidence": 0.8,
    "summary": "one sentence",
    "detail": "why this is real, with file:line evidence from the CURRENT code",
    "suggested_refactor": "concrete next step",
    "introduced_by": "deferred from PR #{{PR_NUMBER}}"
  }
  ```
  (dep_update entries use ecosystem/package/current_version/latest_version;
  modernization entries use modernization_class/current_approach/
  proposed_approach — see apply_improvement.md for both full shapes.)
