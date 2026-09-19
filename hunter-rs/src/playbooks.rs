//! Render worker prompts from playbook templates (playbooks/*.md).
//! Port of hunter/playbooks.py. Templates use {{`SLOT_NAME`}} placeholders
//! substituted with _render. User content is brace-escaped to prevent
//! false-positive assertion failures.

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

/// Substitute {{KEY}} slots in a template. Errors if any {{...}} remain
/// after substitution (typo/missing slot detection, matching Python's
/// assertion) — a half-rendered prompt must never reach a worker.
pub fn render<S: BuildHasher>(template: &str, slots: &HashMap<&str, String, S>) -> Result<String> {
    let mut result = template.to_owned();
    for (k, v) in slots {
        result = result.replace(&format!("{{{{{k}}}}}"), v);
    }
    if let Some(idx) = result.find("{{") {
        let snippet: String = result[idx..].chars().take(60).collect();
        bail!("unfilled placeholder in playbook: {snippet}");
    }
    Ok(result)
}

/// Escape {{ in user content so render's assertion doesn't fire.
pub fn escape_braces(s: &str) -> String {
    s.replace("{{", "{ {")
}

/// Rejected/wontfix findings as a suppression corpus block.
pub fn suppressions_block(suppressions: &[Finding]) -> String {
    if suppressions.is_empty() {
        return "(none yet)".to_owned();
    }
    suppressions
        .iter()
        .map(|s| {
            let fp = escape_braces(&s.fingerprint);
            let reason = escape_braces(
                s.verdict_reason
                    .as_deref()
                    .unwrap_or("(no reason recorded)"),
            );
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
            let fp = escape_braces(&k.fingerprint);
            let summary = escape_braces(&k.summary);
            format!("- {fp} [{}] -- {summary}", k.status)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// Prompt-relevant field keys (playbooks.py:10-40).
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

/// Escape braces in user notes; use a default if empty.
fn notes_or_default(repo_notes: &str, default: &str) -> String {
    if repo_notes.is_empty() {
        default.to_owned()
    } else {
        escape_braces(repo_notes)
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
    escape_braces(&serde_json::to_string_pretty(&subset).unwrap_or_else(|_| "{}".to_owned()))
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
    slots.insert("PR_TITLE", escape_braces(&pr.title));
    slots.insert(
        "PR_BODY",
        escape_braces(if pr.body.is_empty() {
            "(no description)"
        } else {
            &pr.body
        }),
    );
    slots.insert("FEEDBACK", escape_braces(&feedback_blocks(pr, 8000)));
    slots.insert("CHECKS", escape_braces(&checks_lines(pr)));
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
    slots.insert("PR_TITLE", escape_braces(&pr.title));
    slots.insert(
        "PR_BODY",
        escape_braces(if pr.body.is_empty() {
            "(no description)"
        } else {
            &pr.body
        }),
    );
    slots.insert("FEEDBACK", escape_braces(&feedback_blocks(pr, 8000)));
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
    slots.insert("STANDARDS", escape_braces(&standards_content));
    slots.insert("OUT_PATH", out_path.display().to_string());
    slots.insert("MAX_FINDINGS", max_findings.to_string());
    slots.insert("REPO_NOTES", notes);
    render(&template, &slots)
}
