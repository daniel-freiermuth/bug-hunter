Fix exactly ONE verified bug in the worktree at {{WORKTREE}} (repo
{{REPO_NAME}}, branch {{BRANCH}} — already checked out for you). Work only
inside this worktree. NEVER push. NEVER run project-wide formatters.

# Repository Context
{{REPO_NOTES}}

# The finding
```json
{{FINDING_JSON}}
```

# Protocol (evidence FIRST, commit per step)
1. On a fresh attempt, verify the bug still exists at HEAD (code moves). If
   it is already fixed upstream or the finding is demonstrably wrong, STOP:
   write NOT-A-BUG.md explaining the evidence, commit nothing, and end.
   On a resumed attempt, inspect existing commits and reports first. Preserve
   the committed implementation and proof; a fix already present in this
   branch is NOT evidence that the original finding was false. Continue from
   the retained checkpoint, resolving the reported blocker rather than
   rewriting or discarding completed work.
2. Prove it. Climb the evidence ladder as high as the code allows:
   - Rung 1: failing automated test in the repo's existing harness,
     committed first, failure observed and quoted.
   - Rung 2: scripted reproduction (script/curl/REPL trace), recorded
     verbatim in the PR description.
   - Rung 3: written data-flow trace with file:line references enumerating
     every relevant path.
   NEVER introduce a test framework as a side effect. No harness for this
   layer -> rung 2/3.
   Before changing implementation, establish the verification baseline using
   the supported environment and commands required by step 5.
3. Fix it, minimally. No refactors, no drive-by cleanup. Match the repo's
   code style and commit-message convention.
4. Test tier follows the CODE'S SHAPE:
   - pure logic -> extract minimal pure function if needed + unit test;
   - cross-boundary contract -> test both sides;
   - UI-glue -> rung 2/3, note which E2E-level check would cover it.
5. Verify against a baseline BEFORE the change and the patched tree AFTER
   it, using the SAME supported toolchain, real dependency versions, build
   configuration, and test commands. On resume, use the original pre-change
   revision in a separate worktree when baseline evidence is missing; do not
   reset the retained implementation. Run the affected package's checks and
   exercise the changed path, including any added regression. Attempt the
   full affected-toolchain suite and compare its results with the baseline.
   - Affected checks must pass and demonstrate the intended behavior. An
     affected regression, unverified changed path, inaccessible dependency
     required for that proof, or required human decision means BLOCKED.
   - Unrelated failures reproduced on the baseline (including GCC16/header
     or build failures), or unavailable external/multi-host prerequisites
     for unrelated full-suite checks, do NOT by themselves mean BLOCKED or
     invalidate an otherwise verified fix. Record exact commands, versions,
     observed baseline/patched results, missing prerequisites, and which
     coverage remains unavailable in PR-DESCRIPTION.md. If the failure also
     prevents affected-path proof, it IS a blocker until that proof exists.
   - Use the project's actual supported dependencies and environment. NEVER
     fake green with forced includes, logging shims, disabled/hidden/weaker
     tests, or other verification bypasses. Do not fix unrelated bugs merely
     to make the suite green; this assignment grants no such permission.
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
- Missing proof is NOT a false finding. Write NOT-A-BUG.md only when evidence
  establishes that the finding is invalid; if you cannot reach at least rung
  3 with an airtight argument or cannot verify the affected change, write
  BLOCKED.md with the precise missing proof/prerequisite/decision and the
  evidence supporting that conclusion.
- Before ending a blocked attempt, write BOTH BLOCKED.md and
  PR-DESCRIPTION.md, including completed commits, verification evidence,
  limitations, and what is needed to continue. Keep the tree, implementation,
  and both reports; do not delete them or undo valid work. On continuation,
  update the reports and clear BLOCKED.md only once its prerequisite is
  resolved. Do not interpret a stale or scheduler-cleared report as proof of
  resolution: verify the recorded prerequisite.
- If a draft PR is made available for review, explicitly state verification
  limitations and any remaining blocker; a draft is NOT merge-ready or
  shipping-ready evidence.
