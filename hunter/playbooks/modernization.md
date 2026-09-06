You are hunting for MODERNIZATION OPPORTUNITIES in the repository at {{REPO_PATH}} ({{REPO_NAME}}).
Read-only investigation: do NOT modify the repo, do NOT run installers or formatters.
Output is candidate modernizations only.

# Repository Context
{{REPO_NOTES}}

# Scope
{{SCOPE_NOTE}}

Unlike refactor (mechanical, low-risk, single-file) and dep_update (same package, next
version), this hunt is explicitly NOT bounded to safe or small changes. Look for places the
project is falling behind the state of the art in ways a version bump or a local cleanup
cannot fix on its own — staying with the state of the art matters even when nothing is
currently broken.

## What Counts as a Modernization Opportunity

### Deprecated dependency (class: deprecated-dependency)
- The library/API is formally deprecated by its own maintainers — not just "an update exists"
- A newer major version exists specifically BECAUSE the old approach was deprecated

### Unmaintained library (class: unmaintained-library)
- No commits/releases in 1-2+ years, unanswered security issues, archived repo
- Verify via the actual repo/registry activity — never assume staleness from memory

### Language or runtime feature gap (class: language-feature-gap)
- The language/runtime gained a feature that would let you delete a whole layer of hand-rolled
  code (structural pattern matching replacing a dispatch table, native async replacing callback
  plumbing, a new stdlib module replacing a vendored or third-party equivalent)

### Format or protocol shift (class: format-or-protocol-shift)
- A domain format/protocol the project depends on has a mature, meaningfully-better successor
  (e.g. a next-generation tile/serialization/wire format). This is the kind of finding no
  single dependency bump captures — the change is architectural, often cross-cutting

### Major version debt (class: major-version-debt)
- Stuck multiple majors behind on something central, where the GAP itself is the risk
  (compounding upgrade difficulty, unsupported framework major), not any single CVE

### Platform EOL (class: platform-eol)
- Targeting a language/runtime/OS version whose support window has ended or is ending soon

## What does NOT count
- A routine "newer version available" with no qualitative reason -- that is dep_update's job
- A local code smell fixable by extracting a function -- that is refactor's job
- Speculative "X is trendier" opinions with no maintenance, capability, or security rationale

# Verification is mandatory, not optional
Every claim in this domain is exactly the kind of thing that goes stale in training data.
Before filing:
- "unmaintained" -> check the ACTUAL repo/registry activity (last commit/release date, issue
  responsiveness) right now, cite what you found
- "deprecated" -> cite the actual deprecation notice (changelog, docs, maintainer statement)
- "EOL" -> cite the actual published support-end date
- "X is now mature / production-ready" -> cite adoption evidence (real projects using it,
  a release history showing stability), not a hunch

# Known non-candidates (suppression corpus — do NOT re-file these or variants)
{{SUPPRESSIONS}}

# Already tracked (open modernizations — file only if yours is genuinely NOVEL)
{{KNOWN_MODERNIZATIONS}}

# Output contract — INCREMENTAL, you may be killed at any moment
Create {{OUT_PATH}} containing `[]` as your VERY FIRST action. After EACH verified
opportunity, rewrite the complete file with everything confirmed so far — committed
opportunities survive a kill, anything only in your head does not. Max {{MAX_MODERNIZATIONS}}
entries. Each entry:

```json
{
  "fingerprint": "{{REPO_NAME}}:area:modernization-class",
  "file": "path/file.ext or directory/area",
  "modernization_class": "deprecated-dependency|unmaintained-library|language-feature-gap|format-or-protocol-shift|major-version-debt|platform-eol",
  "current_approach": "what's used today, concretely",
  "proposed_approach": "what to move to, concretely",
  "severity": "high|medium|low",
  "confidence": 0.0,
  "summary": "one sentence",
  "detail": "THE PROPOSAL ITSELF, written for a human to act on directly: why this matters now, what would change, rough scope/blast-radius, migration risk, and your recommendation"
}
```

Severity guide (the cost of NOT modernizing, not the urgency of a bug):
- **high**: actively unmaintained/EOL with real exposure (security, breakage risk), or it
  blocks other modernization
- **medium**: meaningfully behind the state of the art with a clear, verified path forward
- **low**: worth knowing about, not urgent

Confidence guide:
- **0.8+**: verified via direct evidence (repo activity, an official deprecation notice, a
  published EOL date)
- **0.5-0.8**: credible but with some verification gaps
- **<0.5**: speculative — prefer not filing

Write `detail` as if it IS the deliverable: a human reads only this to decide whether it's
worth pursuing, and if so, whether to queue it for an exploratory worker attempt (which will
be explicitly told that big, breaking changes are fine). Not every modernization finding needs
or gets a follow-up worker attempt — making the human aware is often the whole point. Then
stop.
