#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_daemon` end to end: the loop, the pause, stopping while paused, and
//! what the daemon binds.
//!
//! Everything else in the suite drives the router or the scheduler
//! directly, so nothing ran the daemon loop itself — mutation testing
//! showed that replacing `run_daemon` with `Ok(())`, or never entering its
//! loop, left every test green. This runs the real thing against a scratch
//! root: a fake `omp` on `PATH`, `HOME` pointed at scratch so omp's real
//! usage database is never read, and a port nothing else uses.
//!
//! It also pins a bug found while rebasing the pause feature: the paused
//! wait listened for a private `ctrl_c()`, which sees only SIGINT, so
//! `systemctl stop` (SIGTERM) on a paused daemon went unanswered until
//! systemd's stop timeout killed it.
//!
//! Both tests stop their daemon by sending SIGTERM to `Pid::this()`. That
//! reaches only the test that sent it: `FakeBins::acquire` refuses to run
//! outside nextest's process-per-test mode.

mod support;

use std::time::{Duration, Instant};

use hunter::config::Config;
use serde_json::Value;
use support::{FakeBins, TempDir};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One HTTP/1.1 request with `Connection: close`; `None` if nothing is
/// listening yet. No client crate: this is the only test that needs one.
async fn http(port: u16, method: &str, path: &str, body: &str) -> Option<(u16, Value)> {
    http_at("127.0.0.1", port, method, path, body).await
}

/// [`http`] against `ip` rather than 127.0.0.1.
async fn http_at(
    ip: &str,
    port: u16,
    method: &str,
    path: &str,
    body: &str,
) -> Option<(u16, Value)> {
    let mut stream = tokio::net::TcpStream::connect((ip, port)).await.ok()?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.ok()?;
    let text = String::from_utf8_lossy(&raw);
    let status = text.split_whitespace().nth(1)?.parse().ok()?;
    let json = text
        .split_once("\r\n\r\n")
        .and_then(|(_, b)| serde_json::from_str(b).ok())
        .unwrap_or(Value::Null);
    Some((status, json))
}

/// Poll until `check` accepts the summary, or fail with `what`.
///
/// Fails at once if the daemon has already returned: waiting out the
/// deadline for a server that is gone only turns a clear failure into a
/// slow one.
async fn wait_for_summary(
    port: u16,
    daemon: &std::thread::JoinHandle<anyhow::Result<()>>,
    what: &str,
    check: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some((200, summary)) = http(port, "GET", "/api/summary", "").await
            && check(&summary)
        {
            return summary;
        }
        assert!(
            !daemon.is_finished(),
            "run_daemon returned while waiting for {what}"
        );
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn a_paused_daemon_reports_it_and_stops_on_sigterm() {
    let bins = FakeBins::acquire("daemon");
    bins.script("omp", "exit 0");
    let root = TempDir::new("daemon-root");
    let _home = bins.env("HOME", root.path());
    let port = free_port();
    std::fs::write(
        root.join("config.json"),
        format!(r#"{{"ompBin": "omp", "serve": {{"port": {port}}}}}"#),
    )
    .unwrap();
    let cfg = Config::load(root.path()).expect("load scratch config");

    // Its own thread and runtime, as `main` gives it: the daemon future is
    // not `Send`, so it cannot be a task on this test's runtime.
    let daemon = std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("daemon runtime")
            .block_on(hunter::daemon::run_daemon(cfg))
    });

    wait_for_summary(port, &daemon, "the daemon to serve /api/summary", |_| true).await;

    let (status, body) = http(port, "POST", "/api/scheduler", r#"{"paused": true}"#)
        .await
        .expect("POST /api/scheduler");
    assert_eq!(status, 200, "body: {body}");

    // Only the loop writes this state, so seeing it proves the loop ran
    // and honoured the pause.
    wait_for_summary(port, &daemon, "the scheduler loop to report paused", |s| {
        s["scheduler_state"]["state"] == "paused"
    })
    .await;

    nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGTERM)
        .expect("send SIGTERM to this test process");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !daemon.is_finished() {
        assert!(
            Instant::now() < deadline,
            "a paused daemon must stop promptly on SIGTERM, as systemctl stop sends it"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon
        .join()
        .expect("run_daemon thread panicked")
        .expect("run_daemon returned an error");
}

/// `serve.host` is what the daemon binds, not a hard-coded loopback.
///
/// Proven without touching a real interface: Linux routes all of
/// 127.0.0.0/8 to loopback, so a daemon told to bind 127.0.0.2 must answer
/// there and refuse 127.0.0.1 -- which a hard-coded bind would accept.
#[tokio::test]
async fn the_daemon_binds_the_configured_address() {
    let bins = FakeBins::acquire("daemon-bind");
    bins.script("omp", "exit 0");
    let root = TempDir::new("daemon-bind-root");
    let _home = bins.env("HOME", root.path());
    let port = free_port();
    std::fs::write(
        root.join("config.json"),
        format!(r#"{{"ompBin": "omp", "serve": {{"host": "127.0.0.2", "port": {port}}}}}"#),
    )
    .unwrap();
    let cfg = Config::load(root.path()).expect("load scratch config");
    let daemon = std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("daemon runtime")
            .block_on(hunter::daemon::run_daemon(cfg))
    });

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some((200, _)) = http_at("127.0.0.2", port, "GET", "/api/summary", "").await {
            break;
        }
        assert!(!daemon.is_finished(), "run_daemon returned before serving");
        assert!(Instant::now() < deadline, "no answer on 127.0.0.2:{port}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "127.0.0.1:{port} answered: the daemon ignored serve.host"
    );

    nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGTERM)
        .expect("send SIGTERM to this test process");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !daemon.is_finished() {
        assert!(Instant::now() < deadline, "daemon did not stop on SIGTERM");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon
        .join()
        .expect("run_daemon thread panicked")
        .expect("run_daemon failed");
}
