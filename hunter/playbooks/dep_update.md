You are checking for OUTDATED DEPENDENCIES in the repository at {{REPO_PATH}} ({{REPO_NAME}}).
Read-only investigation: do NOT modify the repo, do NOT run installers or update commands.
Output is candidate dependency updates only.

# Repository Context
{{REPO_NOTES}}

# Scope
{{SCOPE_NOTE}}

Detect package managers and check for outdated dependencies:
- **npm/yarn/pnpm**: Check `package.json` versions against registry
- **Cargo**: Check `Cargo.toml` versions  
- **pip/Poetry**: Check `requirements.txt`, `pyproject.toml`
- **Go**: Check `go.mod` versions

For each outdated dependency, assess:
- **Update type**: major | minor | patch
- **Security**: Check for known CVEs or security advisories
- **Breaking risk**: Major updates = high risk, patch = low risk
- **Changelog impact**: Review what changed between versions

# What counts as an update candidate
- **Security updates** (ANY version with CVE fix) → always recommend
- **Major updates** (breaking changes likely) → note risk, recommend if maintained
- **Minor updates** (new features, low breaking risk) → recommend
- **Patch updates** (bug fixes only) → always recommend

NOT candidates: Pre-release versions, dependencies pinned for compatibility, internal/vendored packages.

# Known non-candidates (suppression corpus)
Do NOT re-file these or variants unless the specific blocking reason below
has actually been resolved upstream (e.g. a peer dependency range widened,
a removed API was reinstated) — a newer version number alone is not enough.
{{SUPPRESSIONS}}

# Already tracked (open updates — file only if yours is genuinely NOVEL)
{{KNOWN_UPDATES}}

# Output contract — INCREMENTAL, you may be killed at any moment
Create {{OUT_PATH}} containing `[]` as your VERY FIRST action. After EACH
verified update, rewrite the complete file with everything confirmed so far
— committed updates survive a kill, anything only in your head does not.
Max {{MAX_UPDATES}} entries. Each entry:

```json
{
  "fingerprint": "{{REPO_NAME}}:ecosystem:package:current→latest",
  "ecosystem": "npm|cargo|pip|go",
  "package": "package-name",
  "current_version": "1.0.0",
  "latest_version": "2.0.0",
  "update_type": "major|minor|patch",
  "severity": "high|medium|low",
  "confidence": 0.0,
  "summary": "one sentence describing update value",
  "detail": "what changed, breaking changes, why update (with version comparison evidence)",
  "security_advisory": "CVE-2024-XXXX description (if applicable)"
}
```

Severity guide:
- **high**: Security vulnerability, critical bug fix
- **medium**: Major version with valuable features, maintained dependency
- **low**: Minor/patch updates

Confidence guide:
- **0.9+**: Security patch, well-maintained package, clear changelog
- **0.7-0.9**: Minor/patch update, stable package
- **0.5-0.7**: Major update, some breaking changes documented
- **<0.5**: Major update with unclear migration path

Every update MUST include version evidence and changelog review. Then stop.
