Fix exactly ONE verified bug in the worktree at {{WORKTREE}} (repo
{{REPO_NAME}}, branch {{BRANCH}} — already checked out for you). Work only
inside this worktree. NEVER push. NEVER run project-wide formatters.

# The finding
```json
{{FINDING_JSON}}
```

# Protocol (evidence FIRST, commit per step)
1. Verify the bug still exists at HEAD (code moves). If it is already fixed
   or the finding is wrong, STOP: write NOT-A-BUG.md at the worktree root
   explaining why, commit nothing, and end.
2. Prove it. Climb the evidence ladder as high as the code allows:
   - Rung 1: failing automated test in the repo's existing harness,
     committed first, failure observed and quoted.
   - Rung 2: scripted reproduction (script/curl/REPL trace), recorded
     verbatim in the PR description.
   - Rung 3: written data-flow trace with file:line references enumerating
     every relevant path.
   NEVER introduce a test framework as a side effect. No harness for this
   layer -> rung 2/3.
3. Fix it, minimally. No refactors, no drive-by cleanup. Match the repo's
   code style and commit-message convention.
4. Test tier follows the CODE'S SHAPE:
   - pure logic -> extract minimal pure function if needed + unit test;
   - cross-boundary contract -> test both sides;
   - UI-glue -> rung 2/3, note which E2E-level check would cover it.
5. Verify: run the affected package's tests/checks, then the full suite of
   the affected toolchain. All green, or the fix does not ship.
6. COMMIT AFTER EVERY STEP with descriptive messages (proof commit, fix
   commit). You may be killed at any moment — committed work survives,
   uncommitted work dies.

# Deliverables
- Commits on {{BRANCH}} (proof + fix; single commit acceptable when proof is
  prose-only).
- PR-DESCRIPTION.md at the worktree root, NOT COMMITTED (it becomes the PR
  body): what breaks; why (code evidence); the INTENDED behavior and how you
  know it is intended; evidence rung achieved and the proof itself (or where
  it lives); verification performed with observed results; what you
  deliberately did NOT change.
- Prefer showing nothing over shipping uncertainty: if you cannot reach at
  least rung 3 with an airtight argument, write NOT-A-BUG.md or
  BLOCKED.md (with exactly what is missing) instead of a half fix.
