//! Render worker prompts from playbook templates (playbooks/*.md).
//! Port of hunter/playbooks.py. Templates use {{`SLOT_NAME`}} placeholders
//! substituted by `render`, which scans only the template -- user content
//! is inserted verbatim and never reinterpreted.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::path::Path;

use crate::forge::PrView;
use crate::types::{Finding, PrState, Repo};

/// `PLAYBOOK_DIR` = <`project_root>/playbooks` (types.py `PLAYBOOK_DIR`).
pub fn playbook_dir(root: &Path) -> std::path::PathBuf {
    root.join("playbooks")
}

/// Substitute `{{KEY}}` slots in a template, in a single pass.
///
/// Single-pass matters twice. Substituted values are emitted verbatim and
/// never rescanned, so a value containing `{{` can neither be mistaken for
/// an unfilled slot nor be expanded by a later substitution — template
/// injection through PR bodies, repo notes or finding text is structurally
/// impossible rather than escaped away. And because only the TEMPLATE is
/// scanned, an unknown key is a real playbook typo: a half-rendered prompt
/// never reaches a worker.
pub fn render<S: BuildHasher>(template: &str, slots: &HashMap<&str, String, S>) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            let snippet: String = rest[open..].chars().take(60).collect();
            bail!("unterminated placeholder in playbook: {snippet}");
        };
        let key = &after[..close];
        let Some(value) = slots.get(key) else {
            bail!("unfilled placeholder in playbook: {{{{{key}}}}}");
        };
        out.push_str(value);
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Rejected/wontfix findings as a suppression corpus block.
pub fn suppressions_block(suppressions: &[Finding]) -> String {
    if suppressions.is_empty() {
        return "(none yet)".to_owned();
    }
    suppressions
        .iter()
        .map(|s| {
            let fp = &s.fingerprint;
            let reason = s
                .verdict_reason
                .as_deref()
                .unwrap_or("(no reason recorded)");
            format!("- {fp} -- {reason}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Active findings listed for novelty comparison.
pub fn known_block(known: &[Finding]) -> String {
    if known.is_empty() {
        return "(none yet)".to_owned();
    }
    known
        .iter()
        .map(|k| {
            let fp = &k.fingerprint;
            let summary = &k.summary;
            format!("- {fp} [{}] -- {summary}", k.status)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// Prompt-relevant field keys (`playbooks._FINDING_PROMPT_KEYS`).
const FINDING_PROMPT_KEYS: &[&str] = &[
    "type",
    "fingerprint",
    "file",
    "symbol",
    "line",
    "bug_class",
    "severity",
    "confidence",
    "summary",
    "detail",
    "evidence_plan",
    "introduced_by",
    // dep_update fields
    "ecosystem",
    "package",
    "current_version",
    "latest_version",
    "update_type",
    "security_advisory",
    // test_gap fields
    "missing_tests",
    "test_file",
    // refactor fields
    "smell_type",
    "suggested_refactor",
    // modernization fields
    "modernization_class",
    "current_approach",
    "proposed_approach",
    // standards fields
    "standard_section",
];

/// Prompt-relevant fields from a finding, dropping None values.
pub fn finding_subset(f: &Finding) -> serde_json::Value {
    let full = serde_json::to_value(f).unwrap_or_default();
    let Some(map) = full.as_object() else {
        return serde_json::Value::Object(serde_json::Map::new());
    };
    let mut out = serde_json::Map::new();
    for &key in FINDING_PROMPT_KEYS {
        if let Some(v) = map.get(key)
            && !v.is_null()
        {
            out.insert(key.to_owned(), v.clone());
        }
    }
    serde_json::Value::Object(out)
}

// -- Private helpers ---------------------------------------------------------
/// Read a playbook template file.
fn read_template(root: &Path, name: &str) -> Result<String> {
    let path = playbook_dir(root).join(name);
    std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read playbook template {}", path.display()))
}

/// Substitute a default when the repo has no notes.
///
/// Despite the name it once had, this does *not* escape anything: the
/// single-pass `render` never re-examines substituted text (see the
/// module header), so escaping at call sites is unnecessary — and
/// reintroducing it here would corrupt any note legitimately containing
/// braces.
fn notes_or_default(repo_notes: &str, default: &str) -> String {
    if repo_notes.is_empty() {
        default.to_owned()
    } else {
        repo_notes.to_owned()
    }
}

/// Chronological comments + reviews; oldest dropped past ~cap chars.
/// Port of playbooks.py _`feedback_blocks`.
fn feedback_blocks(pr: &PrView, cap: usize) -> String {
    let mut items: Vec<(String, String, String)> = Vec::new();

    for c in &pr.comments {
        let ts = c.created_at.clone();
        let who = c
            .author
            .as_ref()
            .map_or("?", |a| a.login.as_str())
            .to_owned();
        let body = c.body.trim().to_owned();
        items.push((ts, who, body));
    }

    for r in &pr.reviews {
        let state = &r.state;
        let raw_body = r.body.trim();
        let body = if !state.is_empty() && state != "COMMENTED" {
            format!("[review: {state}] {raw_body}").trim().to_owned()
        } else {
            raw_body.to_owned()
        };
        if !body.is_empty() {
            let ts = r.submitted_at.clone();
            let who = r
                .author
                .as_ref()
                .map_or("?", |a| a.login.as_str())
                .to_owned();
            items.push((ts, who, body));
        }
    }

    items.sort();
    let mut blocks: Vec<String> = items
        .iter()
        .map(|(ts, who, body)| format!("### {who} at {ts}\n{body}"))
        .collect();

    let mut dropped = 0usize;
    while blocks.len() > 1 && blocks.iter().map(|b| b.len() + 2).sum::<usize>() > cap {
        blocks.remove(0);
        dropped += 1;
    }
    if dropped > 0 {
        blocks.insert(0, format!("({dropped} older item(s) elided)"));
    }

    if blocks.is_empty() {
        "(no comments or reviews)".to_owned()
    } else {
        blocks.join("\n\n")
    }
}

/// Status check rollup as bullet list.
fn checks_lines(pr: &PrView) -> String {
    let rollup = &pr.status_check_rollup;
    if rollup.is_empty() {
        return "(no checks reported)".to_owned();
    }
    rollup
        .iter()
        .take(30)
        .map(|c| {
            let name = c.name.as_deref().or(c.context.as_deref()).unwrap_or("?");
            let conclusion = c
                .conclusion
                .as_deref()
                .or(c.state.as_deref())
                .unwrap_or("PENDING");
            format!("- {name}: {conclusion}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// JSON-encode finding subset for template injection.
fn finding_json(f: &Finding) -> String {
    let subset = finding_subset(f);
    serde_json::to_string_pretty(&subset).unwrap_or_else(|_| "{}".to_owned())
}

// -- Build functions ---------------------------------------------------------

/// Each build_*_prompt reads the corresponding .md template, builds slots,
/// and calls `render()`. Signatures use typed structs instead of JSON blobs.
pub fn build_hunt_prompt(
    root: &Path,
    repo: &Repo,
    diff_range: &str,
    scope_note: &str,
    suppressions: &[Finding],
    known: &[Finding],
    out_path: &Path,
    max_findings: i64,
    repo_notes: &str,
) -> Result<String> {
    let notes = notes_or_default(
        repo_notes,
        "(No notes yet \u{2014} consider adding conventions/architecture/gotchas \
         as you discover them)",
    );
    let template = read_template(root, "hunt.md")?;
    let mut slots = HashMap::new();
    slots.insert("REPO_PATH", repo.path.clone());
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("DIFF_RANGE", diff_range.to_owned());
    slots.insert("SCOPE_NOTE", scope_note.to_owned());
    slots.insert("SUPPRESSIONS", suppressions_block(suppressions));
    slots.insert("KNOWN_FINDINGS", known_block(known));
    slots.insert("OUT_PATH", out_path.display().to_string());
    slots.insert("MAX_FINDINGS", max_findings.to_string());
    slots.insert("REPO_NOTES", notes);
    render(&template, &slots)
}

pub fn build_fix_prompt(
    root: &Path,
    finding: &Finding,
    worktree: &Path,
    branch: &str,
    repo: &Repo,
    repo_notes: &str,
) -> Result<String> {
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let template = read_template(root, "fix.md")?;
    let mut slots = HashMap::new();
    slots.insert("WORKTREE", worktree.display().to_string());
    slots.insert("BRANCH", branch.to_owned());
    slots.insert("FINDING_JSON", finding_json(finding));
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("REPO_NOTES", notes);
    render(&template, &slots)
}

pub fn build_apply_improvement_prompt(
    root: &Path,
    finding: &Finding,
    worktree: &Path,
    branch: &str,
    repo: &Repo,
    repo_notes: &str,
) -> Result<String> {
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let template = read_template(root, "apply_improvement.md")?;
    let mut slots = HashMap::new();
    slots.insert("WORKTREE", worktree.display().to_string());
    slots.insert("BRANCH", branch.to_owned());
    slots.insert("FINDING_JSON", finding_json(finding));
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("REPO_NOTES", notes);
    render(&template, &slots)
}

pub fn build_apply_modernization_prompt(
    root: &Path,
    finding: &Finding,
    worktree: &Path,
    branch: &str,
    repo: &Repo,
    repo_notes: &str,
) -> Result<String> {
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let template = read_template(root, "apply_modernization.md")?;
    let mut slots = HashMap::new();
    slots.insert("WORKTREE", worktree.display().to_string());
    slots.insert("BRANCH", branch.to_owned());
    slots.insert("FINDING_JSON", finding_json(finding));
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("REPO_NOTES", notes);
    render(&template, &slots)
}

pub fn build_engage_prompt(
    root: &Path,
    finding: &Finding,
    worktree: &Path,
    branch: &str,
    repo: &Repo,
    pr: &PrView,
    ps: &PrState,
    repo_notes: &str,
) -> Result<String> {
    let _ = finding; // engage template does not embed the finding JSON
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let attention = ps
        .needs_attention
        .as_deref()
        .unwrap_or("(none recorded)")
        .to_owned();
    let template = read_template(root, "engage.md")?;
    let mut slots = HashMap::new();
    slots.insert("WORKTREE", worktree.display().to_string());
    slots.insert("BRANCH", branch.to_owned());
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("DEFAULT_BRANCH", repo.default_branch.clone());
    slots.insert("PR_TITLE", pr.title.clone());
    slots.insert(
        "PR_BODY",
        if pr.body.is_empty() {
            "(no description)".to_owned()
        } else {
            pr.body.clone()
        },
    );
    slots.insert("FEEDBACK", feedback_blocks(pr, 8000));
    slots.insert("CHECKS", checks_lines(pr));
    slots.insert("ATTENTION", attention);
    slots.insert("REPO_NOTES", notes);
    render(&template, &slots)
}

pub fn build_harvest_prompt(
    root: &Path,
    finding: &Finding,
    worktree: &Path,
    branch: &str,
    repo: &Repo,
    pr: &PrView,
    pr_number: i64,
    repo_notes: &str,
) -> Result<String> {
    let _ = branch; // harvest template does not use branch directly
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let template = read_template(root, "harvest.md")?;
    let mut slots = HashMap::new();
    slots.insert("WORKTREE", worktree.display().to_string());
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("DEFAULT_BRANCH", repo.default_branch.clone());
    slots.insert("FINDING_JSON", finding_json(finding));
    slots.insert("PR_NUMBER", pr_number.to_string());
    slots.insert("PR_TITLE", pr.title.clone());
    slots.insert(
        "PR_BODY",
        if pr.body.is_empty() {
            "(no description)".to_owned()
        } else {
            pr.body.clone()
        },
    );
    slots.insert("FEEDBACK", feedback_blocks(pr, 8000));
    slots.insert("REPO_NOTES", notes);
    render(&template, &slots)
}

pub fn build_recheck_prompt(
    root: &Path,
    finding: &Finding,
    repo: &Repo,
    out_path: &Path,
    repo_notes: &str,
) -> Result<String> {
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let template = read_template(root, "recheck.md")?;
    let mut slots = HashMap::new();
    slots.insert("REPO_PATH", repo.path.clone());
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("FINDING_JSON", finding_json(finding));
    slots.insert("OUT_PATH", out_path.display().to_string());
    slots.insert("REPO_NOTES", notes);
    render(&template, &slots)
}

/// Analysis-job prompt builders (`test_gap`, `dep_update`, refactor, modernization,
/// standards) share a common signature via the `AnalysisSpec` pattern.
fn build_analysis_prompt(
    root: &Path,
    template_name: &str,
    repo: &Repo,
    scope_note: &str,
    suppressions: &[Finding],
    known: &[Finding],
    out_path: &Path,
    max_items: i64,
    repo_notes: &str,
    known_slot: &str,
    max_slot: &str,
) -> Result<String> {
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let template = read_template(root, template_name)?;
    let mut slots = HashMap::new();
    slots.insert("REPO_PATH", repo.path.clone());
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("SCOPE_NOTE", scope_note.to_owned());
    slots.insert("SUPPRESSIONS", suppressions_block(suppressions));
    slots.insert(known_slot, known_block(known));
    slots.insert("OUT_PATH", out_path.display().to_string());
    slots.insert(max_slot, max_items.to_string());
    slots.insert("REPO_NOTES", notes);
    render(&template, &slots)
}

pub fn build_test_gap_prompt(
    root: &Path,
    repo: &Repo,
    scope_note: &str,
    suppressions: &[Finding],
    known: &[Finding],
    out_path: &Path,
    max_findings: i64,
    repo_notes: &str,
) -> Result<String> {
    build_analysis_prompt(
        root,
        "test_gap.md",
        repo,
        scope_note,
        suppressions,
        known,
        out_path,
        max_findings,
        repo_notes,
        "KNOWN_GAPS",
        "MAX_GAPS",
    )
}

pub fn build_dep_update_prompt(
    root: &Path,
    repo: &Repo,
    scope_note: &str,
    suppressions: &[Finding],
    known: &[Finding],
    out_path: &Path,
    max_findings: i64,
    repo_notes: &str,
) -> Result<String> {
    build_analysis_prompt(
        root,
        "dep_update.md",
        repo,
        scope_note,
        suppressions,
        known,
        out_path,
        max_findings,
        repo_notes,
        "KNOWN_UPDATES",
        "MAX_UPDATES",
    )
}

pub fn build_refactor_prompt(
    root: &Path,
    repo: &Repo,
    scope_note: &str,
    suppressions: &[Finding],
    known: &[Finding],
    out_path: &Path,
    max_findings: i64,
    repo_notes: &str,
) -> Result<String> {
    build_analysis_prompt(
        root,
        "refactor.md",
        repo,
        scope_note,
        suppressions,
        known,
        out_path,
        max_findings,
        repo_notes,
        "KNOWN_REFACTORS",
        "MAX_REFACTORS",
    )
}

pub fn build_modernization_prompt(
    root: &Path,
    repo: &Repo,
    scope_note: &str,
    suppressions: &[Finding],
    known: &[Finding],
    out_path: &Path,
    max_findings: i64,
    repo_notes: &str,
) -> Result<String> {
    build_analysis_prompt(
        root,
        "modernization.md",
        repo,
        scope_note,
        suppressions,
        known,
        out_path,
        max_findings,
        repo_notes,
        "KNOWN_MODERNIZATIONS",
        "MAX_MODERNIZATIONS",
    )
}

pub fn build_standards_prompt(
    root: &Path,
    repo: &Repo,
    scope_note: &str,
    suppressions: &[Finding],
    known: &[Finding],
    out_path: &Path,
    max_findings: i64,
    repo_notes: &str,
) -> Result<String> {
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let template = read_template(root, "standards.md")?;
    // CODING_STANDARDS.md is the entire point of this job type — refuse to
    // build a prompt without it rather than waste tokens on a blind audit.
    let standards_path = root.parent().unwrap_or(root).join("CODING_STANDARDS.md");
    let standards_content = std::fs::read_to_string(&standards_path).with_context(|| {
        format!(
            "CODING_STANDARDS.md not found at {}",
            standards_path.display()
        )
    })?;
    if standards_content.is_empty() {
        bail!(
            "CODING_STANDARDS.md is empty at {}",
            standards_path.display()
        );
    }
    let mut slots = HashMap::new();
    slots.insert("REPO_PATH", repo.path.clone());
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("SCOPE_NOTE", scope_note.to_owned());
    slots.insert("SUPPRESSIONS", suppressions_block(suppressions));
    slots.insert("KNOWN_FINDINGS", known_block(known));
    slots.insert("STANDARDS", standards_content.clone());
    slots.insert("OUT_PATH", out_path.display().to_string());
    slots.insert("MAX_FINDINGS", max_findings.to_string());
    slots.insert("REPO_NOTES", notes);
    render(&template, &slots)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::render;
    use std::collections::HashMap;

    fn slots(pairs: &[(&'static str, &str)]) -> HashMap<&'static str, String> {
        pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect()
    }

    #[test]
    fn substitutes_every_occurrence() {
        let out = render("{{A}}-{{B}}-{{A}}", &slots(&[("A", "1"), ("B", "2")])).unwrap();
        assert_eq!(out, "1-2-1");
    }

    /// The whole point of scanning only the template: a PR body or repo note
    /// containing braces must not look like an unfilled slot.
    #[test]
    fn value_containing_braces_is_not_an_unfilled_placeholder() {
        let out = render(
            "body:\n{{PR_BODY}}",
            &slots(&[("PR_BODY", "use {{ mustache }} like this")]),
        )
        .unwrap();
        assert_eq!(out, "body:\nuse {{ mustache }} like this");
    }

    /// A value must never be rescanned -- otherwise content could inject a
    /// placeholder that a later substitution expands.
    #[test]
    fn value_naming_another_slot_is_not_expanded() {
        let out = render(
            "{{PR_BODY}} {{SECRET}}",
            &slots(&[("PR_BODY", "{{SECRET}}"), ("SECRET", "swordfish")]),
        )
        .unwrap();
        assert_eq!(out, "{{SECRET}} swordfish");
    }

    #[test]
    fn unknown_template_key_is_rejected() {
        let err = render("hello {{TYPO}}", &slots(&[("NAME", "x")])).unwrap_err();
        assert!(err.to_string().contains("TYPO"), "{err}");
    }

    #[test]
    fn unterminated_placeholder_is_rejected() {
        let err = render("hello {{NAME", &slots(&[("NAME", "x")])).unwrap_err();
        assert!(err.to_string().contains("unterminated"), "{err}");
    }

    #[test]
    fn multibyte_around_placeholders_is_preserved() {
        let out = render("→{{A}}←", &slots(&[("A", "café")])).unwrap();
        assert_eq!(out, "→café←");
    }
}
