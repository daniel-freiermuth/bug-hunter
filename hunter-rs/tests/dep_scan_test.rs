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

/// Assert the fake `npx` was actually reached with Renovate's argv, so a
/// `None` in the test below is a decision about the *result* and not the
/// scanner quietly failing to spawn anything.
fn assert_renovate_invoked(bins: &FakeBins) {
    let calls = bins.calls_to("npx");
    assert_eq!(calls.len(), 1, "expected exactly one npx call: {calls:?}");
    assert_eq!(
        calls[0],
        vec![
            "npx".to_owned(),
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
    bins.ok("npx", &format!("{CHATTER}\n{UPDATE_LINE}"));

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
    bins.ok("npx", &format!("{CHATTER}\n{NO_UPDATE_LINE}"));

    let got = scan_repo(work.path(), REPO, 30);

    assert_renovate_invoked(&bins);
    assert!(
        matches!(&got, Some(v) if v.is_empty()),
        "up-to-date repo must be Some(empty), not a model fallback: {got:?}"
    );
}

/// The regression. A failed lookup that still printed a parseable
/// `packageFiles with updates` line used to be accepted, because the gate
/// asked "did it produce output?" instead of "did it succeed?".
#[test]
fn failure_with_output_is_unavailable() {
    let bins = FakeBins::acquire("depscan-fail-loud");
    let work = TempDir::new("depscan-fail-loud-repo");
    // Output on *both* pipes, and the stdout half is exactly the payload
    // the happy-path test above turns into a candidate.
    bins.script(
        "npx",
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
    bins.script("npx", "exit 3");

    let got = scan_repo(work.path(), REPO, 30);

    assert_renovate_invoked(&bins);
    assert!(got.is_none(), "silent failure is unavailable too: {got:?}");
}

/// No `npx` on `PATH` at all: the scanner reports unavailable rather than
/// unwrapping the spawn error.
#[test]
fn missing_binary_is_unavailable() {
    // `isolate()` drops the real PATH: `FakeBins` normally appends it, so
    // a developer's own node would answer and this would pass vacuously.
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
    bins.ok("npx", UPDATE_LINE);
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
        "npx",
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
        "npx",
        &format!("pwd > '{}'\nexit 0", marker.to_string_lossy()),
    );

    let got = scan_repo(&checkout, REPO, 30);

    assert!(got.is_some(), "clean exit is Some: {got:?}");
    let recorded = std::fs::read_to_string(&marker).expect("fake npx recorded its cwd");
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
    bins.ok("npx", &update_line(update_type, vulnerability));
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
