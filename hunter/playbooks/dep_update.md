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

# One entry per change someone would make
An entry is one upgrade a single PR would carry, not one package or one
version:
- **Split the easy bump from the migration.** When a package has both a
  newer release within its current major and a new major, file two entries:
  the in-major update (`<package>-non-major`) and the major
  (`<package>-major`). The first should never wait on the second.
- **Group what must move together**: packages released in lockstep
  (monorepos such as `@typescript-eslint/*` + `typescript-eslint`,
  `@angular/*`), peers that only work at matching versions, and the same
  tool pinned in several places (a `packageManager` field and the CI
  workflow, the same crate in several workspace members). One entry; set
  `package` to the group's name and list every member in `detail` as
  `- name: current → target in file`.
- **Target the newest release** of that unit (newest in-major, newest
  major). A later release of the same unit is the SAME entry, not a new
  one.

# Known non-candidates (suppression corpus)
Do NOT re-file these or variants unless the specific blocking reason below
has actually been resolved upstream (e.g. a peer dependency range widened,
a removed API was reinstated) — a newer version number alone is not enough.
{{SUPPRESSIONS}}

# Already tracked (open updates — file only if yours is genuinely NOVEL)
An entry here with the same unit and current version is the same update
even if its target is older: re-file it with the SAME fingerprint and the
newer target, and hunter updates it in place.
{{KNOWN_UPDATES}}

# Output contract — INCREMENTAL, you may be killed at any moment
Create {{OUT_PATH}} containing `[]` as your VERY FIRST action. After EACH
verified update, rewrite the complete file with everything confirmed so far
— committed updates survive a kill, anything only in your head does not.
Max {{MAX_UPDATES}} entries. Each entry:

```json
{
  "fingerprint": "{{REPO_NAME}}:dep:<package-or-group>-<non-major|major>@<installed version>",
  "ecosystem": "npm|cargo|pip|go",
  "package": "package-name or group name",
  "current_version": "1.0.0",
  "latest_version": "2.0.0",
  "update_type": "major|minor|patch",
  "severity": "high|medium|low",
  "confidence": 0.0,
  "summary": "one sentence describing update value",
  "detail": "what changed, breaking changes, why update (with version comparison evidence); for a group, every member with its versions",
  "security_advisory": "CVE-2024-XXXX description (if applicable)"
}
```

The fingerprint names the unit and the version installed TODAY (the
lockfile's, not the manifest range), never the target: `acme:dep:pnpm-major@11.3.0`.
For a group whose members are at different versions, join them sorted with
`+`.

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
