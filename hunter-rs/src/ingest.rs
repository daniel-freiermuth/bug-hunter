//! Ingest a worker's findings JSON into the store, deduplicating by
//! fingerprint. Port of hunter/ingest.py.

use std::path::Path;

use serde::Serialize;

use crate::domain::{BugClass, FindingType, Severity};
use crate::store::{FindingInsert, Store};

/// Typed result from `ingest_findings`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IngestResult {
    pub inserted: i64,
    pub duplicates: i64,
    pub invalid: i64,
}

/// Per-type required fields (beyond the base fields every type needs).
pub fn type_required_fields(finding_type: FindingType) -> &'static [&'static str] {
    match finding_type {
        FindingType::DepUpdate => &[
            "ecosystem",
            "package",
            "current_version",
            "latest_version",
            "update_type",
        ],
        FindingType::TestGap => &["missing_tests", "test_file"],
        FindingType::Refactor => &["smell_type", "suggested_refactor"],
        FindingType::Modernization => &[
            "modernization_class",
            "current_approach",
            "proposed_approach",
        ],
        FindingType::Standards => &["standard_section", "current_approach", "proposed_approach"],
        FindingType::Bug => &[],
    }
}

/// Fields that must be non-empty lists (rather than non-empty strings).
const LIST_REQUIRED_FIELDS: &[&str] = &["missing_tests"];

/// Ingest findings from a JSON file. `finding_type` = `Some(FindingType::Bug)` for hunts,
/// None for follow-ups (each entry declares its own type).
#[allow(
    clippy::too_many_lines,
    reason = "per-entry validation followed by dedup and insert; the \
              rejection reasons are accumulated into one `IngestResult`, so \
              every branch needs the same mutable tally"
)]
pub async fn ingest_findings(
    store: &Store,
    repo_id: i64,
    findings_path: &Path,
    finding_type: Option<FindingType>,
    job: Option<i64>,
    source_finding: Option<i64>,
) -> IngestResult {
    let mut result = IngestResult::default();
    let text = match std::fs::read_to_string(findings_path) {
        Ok(t) => t,
        Err(e) => {
            let _ = store
                .log_event(
                    "error",
                    &format!(
                        "ingest: unreadable findings file {}: {e}",
                        findings_path.display()
                    ),
                    job,
                    source_finding,
                )
                .await;
            result.invalid = 1;
            return result;
        }
    };
    let entries: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            let _ = store
                .log_event(
                    "error",
                    &format!(
                        "ingest: unreadable findings file {}: {e}",
                        findings_path.display()
                    ),
                    job,
                    source_finding,
                )
                .await;
            result.invalid = 1;
            return result;
        }
    };
    let Some(arr) = entries.as_array() else {
        let _ = store
            .log_event(
                "error",
                &format!(
                    "ingest: findings root is not a list in {}",
                    findings_path.display()
                ),
                job,
                source_finding,
            )
            .await;
        result.invalid = 1;
        return result;
    };

    for (i, f) in arr.iter().enumerate() {
        let entry_type: FindingType = if let Some(ft) = finding_type {
            ft
        } else {
            let raw = f
                .as_object()
                .and_then(|o| o.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let Ok(ft) = raw.parse::<FindingType>() else {
                result.invalid += 1;
                let truncated = serde_json::to_string(f).unwrap_or_default();
                let truncated: String = truncated.chars().take(2000).collect();
                let _ = store
                    .log_event(
                        "error",
                        &format!("ingest: entry {i} has unknown/missing type {raw:?}: {truncated}"),
                        job,
                        source_finding,
                    )
                    .await;
                continue;
            };
            ft
        };
        let entry_type_str = entry_type.to_string();
        let Some(obj) = f.as_object() else {
            result.invalid += 1;
            continue;
        };
        // -- inline validation --
        let problem: Option<String> = (|| {
            match obj.get("fingerprint").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => {}
                _ => return Some("missing fingerprint".to_owned()),
            }
            if entry_type == FindingType::Bug {
                let bc = obj.get("bug_class").and_then(|v| v.as_str()).unwrap_or("");
                if bc.parse::<BugClass>().is_err() {
                    return Some(format!("unknown bug_class {bc:?}"));
                }
                let sev = obj.get("severity").and_then(|v| v.as_str()).unwrap_or("");
                if Severity::parse(sev).is_none() {
                    return Some(format!("unknown severity {sev:?}"));
                }
            } else {
                let sev = obj
                    .get("severity")
                    .and_then(|v| v.as_str())
                    .unwrap_or("medium");
                if Severity::parse(sev).is_none() {
                    return Some(format!("unknown severity {sev:?}"));
                }
                for field in type_required_fields(entry_type) {
                    let value = obj.get(*field);
                    if LIST_REQUIRED_FIELDS.contains(field) {
                        match value {
                            Some(v)
                                if v.is_array()
                                    && !v.as_array().is_none_or(std::vec::Vec::is_empty) => {}
                            _ => {
                                return Some(format!(
                                    "required field {field:?} for type {entry_type:?} must be a non-empty list"
                                ));
                            }
                        }
                    } else {
                        match value.and_then(|v| v.as_str()) {
                            Some(s) if !s.is_empty() => {}
                            _ => {
                                return Some(format!(
                                    "required field {field:?} for type {entry_type:?} must be a non-empty string"
                                ));
                            }
                        }
                    }
                }
            }
            if let Some(c) = obj.get("confidence") {
                let parsed = c
                    .as_f64()
                    .or_else(|| c.as_str().and_then(|s| s.parse::<f64>().ok()));
                match parsed {
                    None => return Some("non-numeric confidence".to_owned()),
                    // `"NaN"` and `"inf"` parse as f64. NaN survives the
                    // clamp below and binds as NULL into a NOT NULL column;
                    // infinity clamps to full confidence. Neither is a
                    // confidence a worker meant, so the entry is invalid.
                    Some(v) if !v.is_finite() => {
                        return Some("non-finite confidence".to_owned());
                    }
                    Some(_) => {}
                }
            }
            None
        })();
        if let Some(problem) = problem {
            result.invalid += 1;
            let truncated = serde_json::to_string(f).unwrap_or_default();
            let truncated: String = truncated.chars().take(2000).collect();
            let _ = store
                .log_event(
                    "error",
                    &format!("ingest: entry {i} invalid ({problem}): {truncated}"),
                    job,
                    source_finding,
                )
                .await;
            continue;
        }
        // -- build typed insert --
        let str_field = |k: &str| obj.get(k).and_then(|v| v.as_str());
        let opt_str = |k: &str| str_field(k).map(std::borrow::ToOwned::to_owned);
        let confidence = obj
            .get("confidence")
            .and_then(|v| {
                v.as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);
        let missing_tests = match obj.get("missing_tests") {
            Some(v) if v.is_array() => Some(v.to_string()),
            Some(v) => v.as_str().map(std::borrow::ToOwned::to_owned),
            None => None,
        };
        // Only bug entries have their `bug_class` validated, so only they
        // may persist one. Storing the raw field for every type lets a
        // refactor entry carrying `"bug_class": "Logic"` write a value the
        // column cannot decode — and the reads are whole-list queries, so
        // that one row fails every later read for the repo, not just its
        // own. Re-parse rather than trust validation: it is the same two
        // lines, and it cannot drift out of step with it.
        let bug_class = (entry_type == FindingType::Bug)
            .then(|| str_field("bug_class").and_then(|s| s.parse::<BugClass>().ok()))
            .flatten()
            .map(|c| c.as_str().to_owned());
        let insert = FindingInsert {
            fingerprint: str_field("fingerprint").unwrap_or("").to_owned(),
            file: str_field("file").unwrap_or("").to_owned(),
            symbol: opt_str("symbol"),
            line: obj.get("line").and_then(serde_json::Value::as_i64),
            // Store the canonical spelling, not what the worker typed.
            // Validation accepts any case (`Severity::parse` lowercases),
            // but the column is decoded back as a `Severity`, whose sqlx
            // representation is lowercase — persisting "HIGH" writes a row
            // that every later read of this repo's findings fails to
            // decode, not just this one.
            severity: Severity::parse(str_field("severity").unwrap_or("medium"))
                .unwrap_or(Severity::Medium)
                .as_str()
                .to_owned(),
            confidence,
            summary: str_field("summary").unwrap_or("").to_owned(),
            detail: opt_str("detail"),
            bug_class,
            evidence_plan: opt_str("evidence_plan"),
            introduced_by: opt_str("introduced_by"),
            ecosystem: opt_str("ecosystem"),
            package: opt_str("package"),
            current_version: opt_str("current_version"),
            latest_version: opt_str("latest_version"),
            update_type: opt_str("update_type"),
            security_advisory: opt_str("security_advisory"),
            missing_tests,
            test_file: opt_str("test_file"),
            smell_type: opt_str("smell_type"),
            suggested_refactor: opt_str("suggested_refactor"),
            modernization_class: opt_str("modernization_class"),
            current_approach: opt_str("current_approach"),
            proposed_approach: opt_str("proposed_approach"),
            standard_section: opt_str("standard_section"),
        };
        match store
            .upsert_finding(repo_id, &insert, &entry_type_str, job)
            .await
        {
            Ok((fid, true)) => {
                result.inserted += 1;
                let event_kind = if entry_type == FindingType::Bug {
                    "hunt"
                } else {
                    &entry_type_str
                };
                let fp = &insert.fingerprint;
                // `job`, not None: this is the event that records the
                // finding coming into existence, so it is the one that
                // most needs to say which job produced it.
                let _ = store
                    .log_event(
                        event_kind,
                        &format!("new {entry_type}: {fp}"),
                        job,
                        Some(fid),
                    )
                    .await;
            }
            Ok((_, false)) => {
                result.duplicates += 1;
            }
            Err(e) => {
                let _ = store
                    .log_event(
                        "error",
                        &format!("ingest: upsert failed for entry {i}: {e}"),
                        job,
                        source_finding,
                    )
                    .await;
                result.invalid += 1;
            }
        }
    }
    result
}
