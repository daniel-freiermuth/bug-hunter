#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `dep_scan::scan_repo` — the Renovate gate.
//!
//! The invariant a review round installed here: an unsuccessful exit means
//! "unavailable", *whatever* the run printed. The original gate only bailed
//! when the output was **also** empty, so a run that died halfway but had
//! already logged a `packageFiles with updates` line was treated as
//! authoritative — and a truncated lookup silently became the repo's full
//! set of dependency findings.
//!
//! The other half of the contract is the `Some(vec![])` / `None`
//! distinction: `None` tells the caller to fall back to the model, which
//! costs a whole AI run, so "renovate ran and found nothing to update" must
//! never collapse into it.

mod support;

use std::path::Path;

use hunter::dep_scan::scan_repo;
use support::{FakeBins, TempDir};

const REPO: &str = "acme/widget";

/// One line of Renovate's `LOG_FORMAT=json` debug stream, carrying a single
/// npm update. This is the shape `parse_renovate_output` mines.
const UPDATE_LINE: &str = r#"{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{"npm":[{"packageFile":"package.json","deps":[{"depName":"left-pad","currentValue":"^1.2.0","datasource":"npm","updates":[{"newVersion":"1.3.0","updateType":"minor"}]}]}]}}"#;

/// The same shape, but every dep is already current: `updates` is empty.
const NO_UPDATE_LINE: &str = r#"{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{"npm":[{"packageFile":"package.json","deps":[{"depName":"left-pad","currentValue":"1.3.0","datasource":"npm","updates":[]}]}]}}"#;

/// Chatter Renovate emits around the useful lines.
const CHATTER: &str = r#"{"name":"renovate","level":30,"msg":"Repository started"}"#;

/// Assert the fake `renovate` was actually reached with its argv, so a
/// `None` in the test below is a decision about the *result* and not the
/// scanner quietly failing to spawn anything.
fn assert_renovate_invoked(bins: &FakeBins) {
    let calls = bins.calls_to("renovate");
    assert_eq!(
        calls.len(),
        1,
        "expected exactly one renovate call: {calls:?}"
    );
    assert_eq!(
        calls[0],
        vec![
            "renovate".to_owned(),
            "--platform=local".to_owned(),
            "--dry-run=lookup".to_owned(),
        ],
    );
}

#[test]
fn success_with_candidates_parses_them() {
    let bins = FakeBins::acquire("depscan-ok");
    let work = TempDir::new("depscan-ok-repo");
    bins.ok("renovate", &format!("{CHATTER}\n{UPDATE_LINE}"));

    let got = scan_repo(work.path(), REPO, 30).expect("renovate succeeded, so Some");

    assert_renovate_invoked(&bins);
    assert_eq!(got.len(), 1, "one dep, one update: {got:?}");
    let c = &got[0];
    assert_eq!(c.package, "left-pad");
    assert_eq!(c.file, "package.json");
    assert_eq!(c.ecosystem, "npm");
    // The range marker is stripped from `current_version` but kept in the
    // fingerprint, which is what dedup keys on downstream.
    assert_eq!(c.current_version, "1.2.0");
    assert_eq!(c.latest_version, "1.3.0");
    assert_eq!(c.update_type, "minor");
    assert_eq!(c.severity, "medium");
    assert_eq!(
        c.fingerprint,
        "acme/widget:npm:left-pad:^1.2.0\u{2192}1.3.0"
    );
}

/// `Some(vec![])` is a real answer: renovate ran, everything is current.
/// Folding it into `None` would send the caller to the model for a repo it
/// has already been told has nothing to do.
#[test]
fn success_with_no_updates_is_empty_not_none() {
    let bins = FakeBins::acquire("depscan-empty");
    let work = TempDir::new("depscan-empty-repo");
    bins.ok("renovate", &format!("{CHATTER}\n{NO_UPDATE_LINE}"));

    let got = scan_repo(work.path(), REPO, 30);

    assert_renovate_invoked(&bins);
    assert!(
        matches!(&got, Some(v) if v.is_empty()),
        "up-to-date repo must be Some(empty), not a model fallback: {got:?}"
    );
}

/// The subtle one: a failed lookup that still printed a parseable
/// `packageFiles with updates` line. The gate has to ask "did it
/// succeed?", not "did it produce output?" — otherwise a half-finished
/// scan is recorded as an authoritative empty result.
#[test]
fn failure_with_output_is_unavailable() {
    let bins = FakeBins::acquire("depscan-fail-loud");
    let work = TempDir::new("depscan-fail-loud-repo");
    // Output on *both* pipes, and the stdout half is exactly the payload
    // the happy-path test above turns into a candidate.
    bins.script(
        "renovate",
        &format!(
            "cat <<'__RENO__'\n{CHATTER}\n{UPDATE_LINE}\n__RENO__\n\
             echo 'FATAL: registry lookup aborted' >&2\nexit 1"
        ),
    );

    let got = scan_repo(work.path(), REPO, 30);

    assert_renovate_invoked(&bins);
    assert!(
        got.is_none(),
        "a non-zero exit is untrustworthy however much it logged: {got:?}"
    );
}

/// Same, with nothing on either pipe — the case the old gate did catch.
/// Kept so a fix that swaps one branch for the other is still caught.
#[test]
fn failure_without_output_is_unavailable() {
    let bins = FakeBins::acquire("depscan-fail-quiet");
    let work = TempDir::new("depscan-fail-quiet-repo");
    bins.script("renovate", "exit 3");

    let got = scan_repo(work.path(), REPO, 30);

    assert_renovate_invoked(&bins);
    assert!(got.is_none(), "silent failure is unavailable too: {got:?}");
}

/// No `renovate` on `PATH` at all: the scanner reports unavailable rather than
/// unwrapping the spawn error.
#[test]
fn missing_binary_is_unavailable() {
    // `isolate()` drops the real PATH: `FakeBins` normally appends it, so
    // a developer's own renovate would answer and this would pass vacuously.
    let bins = FakeBins::acquire("depscan-missing");
    bins.isolate();
    let work = TempDir::new("depscan-missing-repo");

    let got = scan_repo(work.path(), REPO, 30);

    assert!(got.is_none(), "no renovate means fall back to the model");
    assert!(
        bins.calls().is_empty(),
        "nothing should have run: {:?}",
        bins.calls()
    );
}

/// A cwd that does not exist fails at spawn too — the scanner must not
/// treat that as an authoritative "nothing to update".
#[test]
fn unspawnable_scan_is_unavailable() {
    let bins = FakeBins::acquire("depscan-nocwd");
    let work = TempDir::new("depscan-nocwd-repo");
    bins.ok("renovate", UPDATE_LINE);
    let missing = work.join("no-such-checkout");

    let got = scan_repo(&missing, REPO, 30);

    assert!(got.is_none(), "spawn failure is unavailable: {got:?}");
}

/// Renovate's stream is not pure JSON — it interleaves plain text, and a
/// killed or buffered writer can leave a half-written line. Unparseable
/// lines are skipped individually; one of them must not discard the
/// findings on the lines around it.
#[test]
fn malformed_lines_do_not_discard_good_ones() {
    let bins = FakeBins::acquire("depscan-malformed");
    let work = TempDir::new("depscan-malformed-repo");
    let truncated = r#"{"name":"renovate","config":{"npm":[{"packageFile":"pack"#;
    let second = r#"{"name":"renovate","config":{"cargo":[{"packageFile":"Cargo.toml","deps":[{"depName":"serde","currentValue":"1.0.1","datasource":"crate","updates":[{"newVersion":"2.0.0","updateType":"major"}]}]}]}}"#;
    bins.ok(
        "renovate",
        &format!(
            "not json at all\n{UPDATE_LINE}\n{truncated}\n\
             {{\"config\": \"a string, not an object\"}}\n{second}"
        ),
    );

    let got = scan_repo(work.path(), REPO, 30).expect("renovate succeeded, so Some");

    assert_renovate_invoked(&bins);
    let packages: Vec<&str> = got.iter().map(|c| c.package.as_str()).collect();
    assert_eq!(
        packages,
        vec!["left-pad", "serde"],
        "a bad line must skip only itself: {got:?}"
    );
    // The line after the truncated one is the one at risk, so pin it whole.
    let serde = &got[1];
    assert_eq!(serde.ecosystem, "crate");
    assert_eq!(serde.update_type, "major");
    assert_eq!(serde.severity, "high");
}

/// The scanner runs Renovate *in the repo*, since `--platform=local`
/// reads whatever is in the cwd. A scan pointed at the wrong directory
/// would report another checkout's dependencies under this repo's name.
#[test]
fn renovate_runs_inside_the_repo_checkout() {
    let bins = FakeBins::acquire("depscan-cwd");
    let work = TempDir::new("depscan-cwd-repo");
    let checkout = work.subdir("checkout");
    let marker = work.join("cwd.txt");
    bins.script(
        "renovate",
        &format!("pwd > '{}'\nexit 0", marker.to_string_lossy()),
    );

    let got = scan_repo(&checkout, REPO, 30);

    assert!(got.is_some(), "clean exit is Some: {got:?}");
    let recorded = std::fs::read_to_string(&marker).expect("fake renovate recorded its cwd");
    assert_eq!(
        Path::new(recorded.trim()).canonicalize().ok(),
        checkout.canonicalize().ok(),
        "renovate ran in {recorded:?}, not the checkout"
    );
}

/// One update line with a chosen `updateType` and vulnerability flag.
fn update_line(update_type: &str, vulnerability: bool) -> String {
    let vuln = if vulnerability {
        r#","isVulnerabilityAlert":true"#
    } else {
        ""
    };
    format!(
        r#"{{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{{"npm":[{{"packageFile":"package.json","deps":[{{"depName":"left-pad","currentValue":"1.0.0","datasource":"npm","updates":[{{"newVersion":"2.0.0","updateType":"{update_type}"{vuln}}}]}}]}}]}}}}"#
    )
}

fn single_candidate(
    label: &str,
    update_type: &str,
    vulnerability: bool,
) -> hunter::dep_scan::DepCandidate {
    let bins = FakeBins::acquire(label);
    let work = TempDir::new(label);
    bins.ok("renovate", &update_line(update_type, vulnerability));
    let got = scan_repo(work.path(), REPO, 30).expect("renovate succeeded");
    assert_eq!(got.len(), 1, "expected one candidate: {got:?}");
    got.into_iter().next().unwrap_or_else(|| unreachable!())
}

/// The triage table: `updateType` and `isVulnerabilityAlert` decide the
/// severity and confidence every dep finding is ranked by.
///
/// Added after mutation testing showed the parsing tests pinned only THAT
/// a candidate comes back, not how it is graded — six mutants that
/// rewrote this table survived, including one that downgrades a security
/// advisory to `low`.
#[test]
fn update_type_and_vulnerability_decide_severity() {
    for (ut, vuln, severity) in [
        ("major", false, "high"),
        ("major", true, "high"),
        ("minor", false, "medium"),
        ("minor", true, "medium"),
        // Neither major nor minor: the advisory flag is the only thing
        // that lifts it off the floor.
        ("patch", false, "low"),
        ("patch", true, "high"),
        ("replacement", false, "low"),
        ("replacement", true, "high"),
    ] {
        let c = single_candidate(&format!("sev-{ut}-{vuln}"), ut, vuln);
        assert_eq!(
            c.severity, severity,
            "updateType={ut} isVulnerabilityAlert={vuln} should be {severity}"
        );
    }
}

/// Confidence is graded separately from severity: a patch is the safest
/// bump even though it is the lowest severity.
#[test]
fn update_type_decides_confidence() {
    for (ut, confidence) in [
        ("patch", 0.95),
        ("minor", 0.85),
        ("major", 0.7),
        ("replacement", 0.7),
    ] {
        let c = single_candidate(&format!("conf-{ut}"), ut, false);
        assert!(
            (c.confidence - confidence).abs() < f64::EPSILON,
            "updateType={ut} should be {confidence}, got {}",
            c.confidence
        );
    }
}

/// Renovate's housekeeping update types are all reported as `patch`.
/// Leaving them unnormalised would give each its own severity and
/// confidence bucket, and none of them match the table above.
#[test]
fn housekeeping_update_types_normalise_to_patch() {
    for ut in [
        "pin",
        "pinDigest",
        "digest",
        "lockFileMaintenance",
        "lockfileUpdate",
    ] {
        let c = single_candidate(&format!("norm-{ut}"), ut, false);
        assert_eq!(c.update_type, "patch", "{ut} should normalise to patch");
        assert_eq!(c.severity, "low");
        assert!((c.confidence - 0.95).abs() < f64::EPSILON);
    }
}

/// A `renovate` the scanned repo ships must never run.
///
/// This is what `npx renovate` did: npx resolves from the current
/// directory's `node_modules/.bin` first, and the current directory is the
/// checkout. The repo copy here is also reachable through a relative
/// `PATH` entry and an absolute one pointing into the checkout -- the two
/// other ways the checkout's copy could win -- and the installed one sits
/// behind both, so a scanner that honoured either would run the repo's.
#[test]
fn a_renovate_shipped_by_the_scanned_repo_never_runs() {
    let bins = FakeBins::acquire("depscan-hijack");
    let work = TempDir::new("depscan-hijack-repo");
    let marker = work.join("HIJACKED");
    let repo_bin = work.subdir("node_modules/.bin");
    std::fs::create_dir_all(&repo_bin).unwrap();
    let evil = repo_bin.join("renovate");
    std::fs::write(&evil, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&evil, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bins.ok("renovate", NO_UPDATE_LINE);
    let installed = std::env::var("PATH").unwrap();
    let hostile = format!("node_modules/.bin:{}:{installed}", repo_bin.display());
    let _path = bins.env("PATH", Path::new(&hostile));

    let got = scan_repo(work.path(), REPO, 30);

    assert!(
        !marker.exists(),
        "the scanned repo's own renovate ran -- repo content executed as the daemon"
    );
    assert_renovate_invoked(&bins);
    assert!(matches!(&got, Some(v) if v.is_empty()), "{got:?}");
}

/// When the only `renovate` available is the repo's, there is no scan --
/// and certainly no download: the caller falls back to the model.
#[test]
fn a_repo_shipped_renovate_is_not_a_fallback() {
    let bins = FakeBins::acquire("depscan-only-repo");
    bins.isolate();
    let work = TempDir::new("depscan-only-repo-repo");
    let marker = work.join("HIJACKED");
    let repo_bin = work.subdir("node_modules/.bin");
    std::fs::create_dir_all(&repo_bin).unwrap();
    let evil = repo_bin.join("renovate");
    std::fs::write(&evil, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&evil, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let _path = bins.env("PATH", &repo_bin);

    let got = scan_repo(work.path(), REPO, 30);

    assert!(!marker.exists(), "the repo's renovate ran");
    assert!(
        got.is_none(),
        "no installed renovate means fall back: {got:?}"
    );
}

/// Renovate runs with a scrubbed environment: a token the daemon holds is
/// not inherited, while what a Node process needs to start still is.
#[test]
fn renovate_does_not_inherit_the_daemons_secrets() {
    let bins = FakeBins::acquire("depscan-env");
    let work = TempDir::new("depscan-env-repo");
    let seen = work.join("env.txt");
    bins.script(
        "renovate",
        &format!("env > '{}'\necho '{NO_UPDATE_LINE}'", seen.display()),
    );
    let _token = bins.env("GH_TOKEN", Path::new("s3cret-token"));
    let _home = bins.env("HOME", work.path());

    let got = scan_repo(work.path(), REPO, 30);

    assert!(got.is_some(), "{got:?}");
    let env = std::fs::read_to_string(&seen).unwrap();
    assert!(
        !env.contains("s3cret-token"),
        "GH_TOKEN leaked into renovate:\n{env}"
    );
    assert!(
        env.lines().any(|l| l.starts_with("HOME=")),
        "HOME must be forwarded:\n{env}"
    );
    assert!(env.contains("LOG_FORMAT=json"), "{env}");
}

/// The `PATH` Renovate itself runs with cannot reach into the checkout.
///
/// The installed `renovate` is a Node script that starts with
/// `#!/usr/bin/env node`, so the child looks `node` up on its own `PATH`,
/// from inside the checkout. A relative entry like `node_modules/.bin`
/// would find a `node` the repo ships. Here the fake renovate calls `node`
/// the same way, and the repo ships a `node` that leaves a marker.
#[test]
fn renovates_own_path_lookups_cannot_reach_the_checkout() {
    let bins = FakeBins::acquire("depscan-child-path");
    let work = TempDir::new("depscan-child-path-repo");
    let marker = work.join("HIJACKED");
    let repo_bin = work.subdir("node_modules/.bin");
    std::fs::create_dir_all(&repo_bin).unwrap();
    let evil = repo_bin.join("node");
    std::fs::write(&evil, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&evil, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bins.script(
        "renovate",
        &format!("node --version >/dev/null 2>&1\necho '{NO_UPDATE_LINE}'"),
    );
    let installed = std::env::var("PATH").unwrap();
    let hostile = format!("node_modules/.bin:{installed}");
    let _path = bins.env("PATH", Path::new(&hostile));

    let got = scan_repo(work.path(), REPO, 30);

    assert!(got.is_some(), "{got:?}");
    assert!(
        !marker.exists(),
        "renovate's child PATH resolved into the checkout and ran the repo's node"
    );
}
