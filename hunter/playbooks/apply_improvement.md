Apply exactly ONE suggested improvement in the worktree at {{WORKTREE}} (repo
{{REPO_NAME}}, branch {{BRANCH}} — already checked out for you). Work only
inside this worktree. NEVER push. NEVER run project-wide formatters.

This playbook handles IMPROVEMENTS (dep_update, test_gap, refactor), NOT bugs.
For bugs, see fix.md.

# Repository Context
{{REPO_NOTES}}

# The improvement
```json
{{FINDING_JSON}}
```

# Protocol (assess FIRST, commit per step)

## 1. Assess value and feasibility

Read the finding and determine if the improvement is:
- **Valuable**: Does it add real value (security, performance, coverage, maintainability)?
- **Safe**: Low risk of breaking existing functionality?
- **Actionable**: Can you apply it now without major blockers?

If NOT valuable or safe, STOP: write DECLINED.md explaining why (e.g., "change
too risky", "already covered elsewhere", "dependency pinned for compatibility").

If blocked by external factors you can't resolve (conflicting patches, missing
information, requires architectural decisions), STOP: write BLOCKED.md with
exactly what's missing.

## 2. Apply the improvement

### For `type = 'dep_update'`:

a. **Check for local patches** in patches/, .patch files, or vendored code
b. **Upgrade the dependency** to the target version
c. **Handle patches**:
   - If patches exist, try to rebase/update them for the new version
   - If patches conflict or fail, document in BLOCKED.md
   - If patches are no longer needed (fix upstreamed), remove them
d. **Install and verify** the new version resolves correctly
e. **Commit**: "deps: upgrade {package} from {old} to {new}"

### For `type = 'test_gap'`:

a. **Verify the gap** - confirm the contract/edge case is untested
b. **Add tests** covering the missing scenario
   - Use existing test harness and conventions
   - NEVER introduce new test frameworks as a side effect
c. **Run tests** - verify new tests pass and existing tests still pass
d. **Commit**: "test: add coverage for {contract/edge-case}"

### For `type = 'refactor'`:

a. **Verify the smell** - confirm the code issue exists
b. **Apply refactoring** (extract method, remove duplication, simplify)
   - Keep changes minimal and mechanical
   - Match existing code style
c. **Run tests** - verify no behavior change
d. **Commit**: "refactor: {what} in {where}"

## 3. Verify

Run the affected package's tests/checks. For dependency updates, also:
- Check that lockfiles updated correctly
- Verify no new deprecation warnings
- Spot-check one usage of the upgraded dependency

All green, or the improvement does not ship.

## 4. Commit discipline

COMMIT AFTER EVERY STEP with descriptive messages. You may be killed at any
moment — committed work survives, uncommitted work dies.

# Deliverables

- **Commits on {{BRANCH}}** (upgrade/test/refactor commits)
- **PR-DESCRIPTION.md** at the worktree root, NOT COMMITTED (becomes PR body):
  - What improved (dep upgrade, test coverage, refactoring)
  - Why valuable (security, correctness, maintainability)
  - What changed (version bump + patch rebase, new test cases, simplified code)
  - Verification performed with observed results
  - What you deliberately did NOT change

OR if not actionable:

- **DECLINED.md**: Why the improvement isn't worth doing (already covered, too
  risky, not valuable enough, dependency pinned for compatibility)
- **BLOCKED.md**: What's blocking you (patch conflicts, missing arch decisions,
  breaking changes need human review) with exactly what is needed to unblock

# Improvement-specific guidance

## Dependency updates with patches

If local patches exist:
1. Note patch purpose (bug fix, API adaptation, feature backport)
2. Check if target version makes patch obsolete (fix upstreamed)
3. If patch still needed, rebase to new version:
   - Apply patch to new version
   - Resolve conflicts
   - Test that patch still serves its purpose
4. If patch conflicts and you can't resolve: BLOCKED.md "patch rebase failed,
   needs manual resolution" + show conflict details

## Test gaps

Focus on:
- Edge cases and boundaries (empty, null, first/last, max)
- Error paths (invalid input, failure states)
- Contracts between components (API shape, invariants)

NOT:
- Tests for implementation details
- Redundant coverage of already-tested paths
- Tests that would require major scaffolding

## Refactoring

Safe refactorings only:
- Extract duplicate code
- Simplify complex conditionals
- Remove dead code (verify unused with grep)
- Rename for clarity (use LSP rename if available)

NOT:
- Architectural changes
- Pattern shifts
- Abstraction introduction without clear duplication

# Success criteria

The improvement is:
- **Applied correctly** (upgraded, tested, refactored)
- **Verified working** (tests pass, no regressions)
- **Well-documented** (clear PR description)
- **Minimal** (only what the finding suggests, no scope creep)

OR explicitly declined/blocked with clear reasoning.
