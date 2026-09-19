//! Static dependency scanner using Renovate's local platform.
//!
//! Zero AI tokens — Renovate handles ecosystem detection, registry
//! queries, semver classification, and advisory lookup. Falls back
//! to None when Renovate isn't installed or fails.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::util::{drain_pipe, join_pipes, kill_tree};

/// One update candidate, matching the `dep_update` finding schema.
#[derive(Debug, Clone)]
pub struct DepCandidate {
    pub fingerprint: String,
    pub file: String,
    pub ecosystem: String,
    pub package: String,
    pub current_version: String,
    pub latest_version: String,
    pub update_type: String,
    pub severity: String,
    pub confidence: f64,
    pub summary: String,
    pub detail: String,
}

/// Run Renovate in local dry-run mode and return update candidates.
/// Returns None on failure (not installed, timeout, parse error) —
/// the caller falls back to the AI-based analysis job.
pub fn scan_repo(repo_path: &Path, repo_name: &str, timeout_s: u64) -> Option<Vec<DepCandidate>> {
    let mut cmd = Command::new("npx");
    cmd.args(["renovate", "--platform=local", "--dry-run=lookup"])
        .current_dir(repo_path)
        .env("LOG_FORMAT", "json")
        .env("LOG_LEVEL", "debug")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        // npx execs node which execs renovate: the timeout below can only
        // be enforced against the whole tree.
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("dep_scan: renovate not available for {repo_name}: {e}");
            return None;
        }
    };

    // Renovate is extremely chatty at debug level; drain both pipes from a
    // reader thread each or it wedges on a full pipe buffer mid-lookup.
    let so = drain_pipe(child.stdout.take());
    let se = drain_pipe(child.stderr.take());

    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_s);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = join_pipes(so, se);
                // A non-zero exit means the lookup is untrustworthy even when
                // it logged plenty: fall back to the AI job. An empty Some()
                // still means "renovate ran, nothing to update".
                if !status.success() {
                    tracing::warn!(
                        "dep_scan: renovate failed (rc={:?}) for {repo_name}",
                        status.code()
                    );
                    return None;
                }
                let candidates = parse_renovate_output(&output, repo_name);
                tracing::info!(
                    "dep_scan: {repo_name} — {} update candidates from renovate",
                    candidates.len()
                );
                return Some(candidates);
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    kill_tree(&mut child);
                    let _ = join_pipes(so, se);
                    tracing::warn!("dep_scan: renovate timeout for {repo_name}");
                    return None;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(_) => {
                kill_tree(&mut child);
                let _ = join_pipes(so, se);
                return None;
            }
        }
    }
}

fn parse_renovate_output(output: &str, repo_name: &str) -> Vec<DepCandidate> {
    let mut candidates = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for line in output.lines() {
        let obj: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // One check, not two: the `is_object()` guard this replaces was
        // redundant with the `as_object()` below, so no input could tell
        // them apart.
        let Some(config_obj) = obj.get("config").and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (manager, files) in config_obj {
            let Some(files_arr) = files.as_array() else {
                continue;
            };
            for pf in files_arr {
                let Some(pf_obj) = pf.as_object() else {
                    continue;
                };
                let pkg_file = pf_obj
                    .get("packageFile")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let Some(deps) = pf_obj.get("deps").and_then(|v| v.as_array()) else {
                    continue;
                };
                for dep in deps {
                    let dep_name = dep
                        .get("depName")
                        .or_else(|| dep.get("packageName"))
                        .and_then(|v| v.as_str());
                    let Some(dep_name) = dep_name else { continue };
                    let current = dep
                        .get("currentValue")
                        .or_else(|| dep.get("currentVersion"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let datasource = dep
                        .get("datasource")
                        .and_then(|v| v.as_str())
                        .unwrap_or(manager);

                    let Some(updates) = dep.get("updates").and_then(|v| v.as_array()) else {
                        continue;
                    };
                    for u in updates {
                        let new_version = u
                            .get("newVersion")
                            .or_else(|| u.get("newValue"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("?");
                        // Normalise BEFORE grading. Renovate's
                        // housekeeping types mean "this is a patch", and
                        // the record says so — grading them off the raw
                        // value gave two findings both labelled `patch`
                        // different confidences, with pin/digest scored
                        // 0.7, the same as a major bump.
                        let update_type = normalize_update_type(
                            u.get("updateType")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown"),
                        );

                        let fp = format!(
                            "{repo_name}:{datasource}:{dep_name}:{current}\u{2192}{new_version}"
                        );
                        if !seen.insert(fp.clone()) {
                            continue;
                        }

                        let severity = match update_type {
                            "major" => "high",
                            "minor" => "medium",
                            _ if u
                                .get("isVulnerabilityAlert")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false) =>
                            {
                                "high"
                            }
                            _ => "low",
                        };
                        let confidence = match update_type {
                            "patch" => 0.95,
                            "minor" => 0.85,
                            _ => 0.7,
                        };
                        let current_clean =
                            current.trim_start_matches(|c: char| "^~>=<".contains(c));

                        candidates.push(DepCandidate {
                            fingerprint: fp,
                            file: pkg_file.clone(),
                            ecosystem: datasource.to_owned(),
                            package: dep_name.to_owned(),
                            current_version: current_clean.to_owned(),
                            latest_version: new_version.to_owned(),
                            update_type: update_type.to_owned(),
                            severity: severity.to_owned(),
                            confidence,
                            summary: format!(
                                "{dep_name}: {update_type} update {current} \u{2192} {new_version}"
                            ),
                            detail: format!(
                                "{dep_name} ({datasource}/{manager})\n\
                                 Current: {current}\n\
                                 Available: {new_version}\n\
                                 Type: {update_type}"
                            ),
                        });
                    }
                }
            }
        }
    }
    candidates
}

fn normalize_update_type(ut: &str) -> &str {
    match ut {
        "pin" | "pinDigest" | "digest" | "lockFileMaintenance" | "lockfileUpdate" => "patch",
        _ => ut,
    }
}
