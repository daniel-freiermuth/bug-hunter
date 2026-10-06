You are following up on a MODERNIZATION proposal in the worktree at {{WORKTREE}} (repo
{{REPO_NAME}}, branch {{BRANCH}} — already checked out for you). Work only inside this
worktree. NEVER push. NEVER run project-wide formatters.

This playbook is deliberately NOT apply_improvement.md's contract. A human explicitly queued
this proposal knowing it may require a big, breaking, or multi-file change — that is expected
here, not a reason to decline.

# Repository Context
{{REPO_NOTES}}

# The proposal
```json
{{FINDING_JSON}}
```

# Protocol

## 1. Re-verify before doing anything

On continuation, inspect existing commits and reports first. Preserve the
committed plan/spike/migration and its proof; continue from the retained
checkpoint to resolve the blocker rather than rewriting or discarding work.
An implementation already present on this branch does not invalidate the
original proposal.

The hunt-time claims (deprecation, staleness, EOL, "X is mature now") may be stale by the time
this runs — training data and even a recent hunt pass can be wrong. Re-check with real
commands or sources: registry/repo activity, actual deprecation notices, actual published EOL
dates. If re-verification shows the premise no longer holds (the library got a new maintainer,
the format never gained real adoption, the EOL date moved), STOP: write DECLINED.md saying so,
with the re-verification evidence.

## 2. Decide the right deliverable for THIS proposal's actual scope

Before any code change in the chosen deliverable, establish the verification
baseline using the supported environment and commands required by step 3.

There is no single correct shape here — pick the one that is honest about how far a single
pass can responsibly go, and what is still a judgment call for a human:

### (a) Migration plan only — the default for anything genuinely large
If a full migration cannot be responsibly attempted in one pass (spans many files, needs a
design decision, has unclear rollback), do NOT attempt the code change. Instead, commit ONLY
a plan document under `docs/` named after the finding's fingerprint or topic (e.g.
`docs/MODERNIZATION-<topic-slug>.md`):
- Current state, target state, concrete migration steps in order, what breaks and for whom,
  an effort estimate, and your recommendation
- This MUST be a real commit (a shipped PR always needs at least one) — do not leave it
  uncommitted and only describe it in the PR body
- Prefix the PR title with `[RFC]` so it is unambiguous this needs a decision, not a merge

### (b) Proof of concept / spike — when a meaningful chunk IS tractable
If you can demonstrate the migration working end-to-end for a representative slice (even if
incomplete), do that: commit the working spike, and be explicit in the PR description about
exactly what is NOT done yet. Breaking existing behavior in the spike is fine as long as it is
disclosed, not discovered later.

### (c) Full migration — only when genuinely tractable in scope and you have high confidence
Same commit-per-step, test-as-you-go discipline as any other fix. Breaking changes are allowed
here — call them out explicitly in the PR description; do not downplay them.

### CI/CD gaps specifically: usually (c), not (a)

A `ci-cd-gap` finding is a different shape from a code migration -- adding or wiring up a
workflow file is normally additive and isolated from application code, not a multi-file
rewrite with unclear rollback. Default to (c) full implementation for wiring up
lint/test/build gates; do NOT drop to an RFC plan document just because "modernization
findings often do." The exceptions:
- **Release/publish automation is the higher-risk part of this class** — it touches external
  registries, app stores, or secrets you don't have and shouldn't invent. For that piece
  specifically, prefer (a) a plan document (what secrets/environment a human needs to
  provision) or (b) a dry-run-only spike (build the release artifact, do not attempt to
  actually publish it) — never fabricate credentials or assume a secret already exists.
- Mirror the project's OWN existing tooling. Before writing a single line of workflow YAML,
  find how this repo already builds/tests/lints/releases by hand (Makefile, justfile,
  `package.json` scripts, README/CONTRIBUTING instructions) and wire up exactly those
  commands — do not introduce a generic template pipeline with commands the project doesn't
  actually use.
- **Verify every command locally before it goes in the workflow file.** Run the exact
  lint/test/build/release-dry-run commands you're about to add, in this worktree, right now,
  and quote the results. A new workflow file typically will not execute until this PR is
  merged (GitHub/GitLab do not run brand-new pipeline definitions against the PR that adds
  them the same way they run existing ones) — your local run is the only verification
  evidence that will exist before merge, so it is not optional.

Whichever you pick, COMMIT PER STEP: you may be killed at any moment, and committed work
survives while anything only in your head does not. Every path — including (a) — ends with at
least one commit; a PR needs a diff to open.

## 3. Verify whatever you actually changed

Before changing code, establish the pre-change baseline. After the change,
run affected checks and exercise the changed path, including added regressions,
with the SAME supported toolchain, actual dependencies, configuration, and
commands. On resume, obtain missing baseline evidence from the original
revision in a separate worktree, never by resetting the retained implementation.
Attempt the full affected-toolchain suite and compare baseline/patched results.

Affected-path proof is mandatory for the chosen deliverable: a spike must
demonstrate its promised representative slice, and a full migration must prove
its affected behavior (including explicitly intended breaking changes).
Unintended affected regressions, unverified affected changes, inaccessible
dependencies required for that proof, or decisions required to proceed mean
BLOCKED. Do not call a blocked implementation a completed spike or full migration.

Unrelated failures reproduced on the baseline (including GCC16/header/build
failures) or unavailable external/multi-host prerequisites for unrelated
full-suite checks are not automatic blockers. Record exact commands, toolchain
and dependency versions, baseline/patched outcomes, missing prerequisites,
and unavailable coverage in PR-DESCRIPTION.md. If these failures prevent the
affected-path proof, they ARE blockers. For CI/CD work, the exact new workflow
commands are affected checks and must be verified as required in step 2.

NEVER fake green with forced includes, logging shims, disabled/hidden/weaker
tests, or other verification bypasses. Use actual supported dependencies and
do not infer permission to fix unrelated bugs merely to make the suite pass.
If you only wrote a plan, there is no code to run, but every factual claim must
still have re-verified evidence, not merely restate the hunt-time claim. A plan
that explicitly requests a decision is not proof of a completed migration.

# Deliverables

- **Commits on {{BRANCH}}** — at least one in every path, even (a) (the plan document itself)
- **PR-DESCRIPTION.md** at the worktree root, NOT COMMITTED (becomes the PR body): which of
  (a) / (b) / (c) you chose and why, the plan or the verification results, and — critically —
  exactly what a human still needs to decide
- If you chose (a), start the PR-DESCRIPTION.md title line with `[RFC]` so draft-PR review
  expectations are set correctly from the subject line alone

OR if re-verification showed the premise no longer holds:

- **DECLINED.md**: what you re-verified and why it is no longer worth pursuing, with the
  verification command/output backing the claim

OR if a required prerequisite/decision or affected-path proof is missing:

- **BLOCKED.md AND PR-DESCRIPTION.md**: write both before ending, recording
  completed commits, observed verification, limitations, the precise blocker,
  and what resolves it. Preserve valid committed work, the worktree, and both
  reports; do not delete reports or undo implementation. On continuation,
  update them and clear BLOCKED.md only once the prerequisite is resolved.
  A stale or scheduler-cleared report does not prove resolution.
- Missing proof is not a verified reason to decline the proposal. DECLINED.md
  requires evidence that the actual premise is invalid or unwanted, as above.
- An optional draft PR must disclose limitations and remaining blockers;
  draft status is NOT merge-ready or shipping-ready evidence.

# What this is not

Not a license to guess. Every "unmaintained" / "deprecated" / "EOL" / "now mature" claim
inherited from the finding gets re-verified here too — the hunt could be stale by the time
this runs, and a wrong migration recommendation is worse than no recommendation.
