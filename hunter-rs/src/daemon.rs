//! Daemon loop — UI server + scheduler loop + usage prober, one process,
//! three tokio tasks. Port of server.py daemon/_`compute_sleep_s`/
//! _`usage_prober_loop`/_`reconcile_and_log`/_`describe_cycle`/_`acquire_lockfile`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

use crate::backend::Backend;
use crate::config::Config;
use crate::domain::JobState;
use crate::scheduler::CycleSummary;
use crate::server::{AppState, router};
use crate::store::Store;

const USAGE_PROBE_TICK_S: u64 = 60;
const PR_SYNC_INTERVAL_S: f64 = 300.0; // 5 min, matching Python

/// Exclusive lockfile on <`work_root>/hunter.lock` (server.py:58-81).
/// Returns the open File (holds the lock while alive).
pub fn acquire_lockfile(work_root: &Path) -> anyhow::Result<std::fs::File> {
    let lock_path = work_root.join("hunter.lock");
    std::fs::create_dir_all(work_root)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    file.try_lock().map_err(|_| {
        anyhow::anyhow!(
            "another hunter process is already running (lock: {})\n  \
             check: ps aux | grep '[h]unter --root'\n  \
             or:    systemctl --user status hunter.service",
            lock_path.display()
        )
    })?;
    Ok(file)
}

/// Recover orphaned jobs/findings from a crashed prior process.
pub async fn reconcile_and_log(store: &Store) -> anyhow::Result<()> {
    let (findings, jobs) = store.reconcile_orphaned_jobs().await?;
    for f in &findings {
        let _ = store
            .log_event(
                "error",
                &format!(
                    "reconciled #{} stuck 'fixing' -> 'queued' -- prior process died mid-fix",
                    f.id
                ),
                None,
                Some(f.id),
            )
            .await;
    }
    for j in &jobs {
        let _ = store
            .log_event(
                "error",
                &format!(
                    "reconciled orphaned {} job #{} (finding {:?}) -- prior process died mid-job",
                    j.kind, j.id, j.finding_id
                ),
                Some(j.id),
                j.finding_id,
            )
            .await;
    }
    if !findings.is_empty() || !jobs.is_empty() {
        tracing::warn!(
            "reconciled {} stuck finding(s) + {} orphaned job(s)",
            findings.len(),
            jobs.len()
        );
    }
    Ok(())
}

/// (`state_label`, detail) for `scheduler_state` (server.py:728-761).
pub fn describe_cycle(summary: &CycleSummary) -> (String, String) {
    use std::fmt::Write;
    if let Some(e) = summary.error.as_deref() {
        return ("error".into(), e.chars().take(200).collect());
    }
    if let Some(v) = summary.idle.as_deref() {
        return ("idle".into(), v.to_owned());
    }
    if let Some(v) = summary.skipped.as_deref() {
        return ("idle".into(), v.to_owned());
    }
    if let Some(d) = summary.denied.as_deref() {
        return ("denied".into(), format!("last: {d}"));
    }
    let state = summary.state;
    let kind = summary.kind.map_or("", super::domain::JobKind::as_str);
    if matches!(
        state,
        Some(JobState::Done | JobState::Killed | JobState::Failed)
    ) && !kind.is_empty()
    {
        let target = if let Some(fid) = summary.finding_id {
            format!("#{fid}")
        } else if let Some(repo) = summary.repo.as_deref() {
            format!("({repo})")
        } else {
            "(?)".into()
        };
        let outcome = summary.outcome.as_deref();
        let mut bits = format!("{kind} {target}");
        if let Some(o) = outcome {
            let _ = write!(bits, " -> {o}");
        } else if !matches!(state, Some(JobState::Done)) {
            let _ = write!(
                bits,
                " {}",
                state.map_or("", super::domain::JobState::as_str)
            );
        }
        return ("idle".into(), format!("last: {bits}"));
    }
    ("idle".into(), "cycle produced no actionable outcome".into())
}

/// How long the daemon should sleep after a cycle (server.py:934-995).
pub async fn compute_sleep_s(store: &Store, summary: &CycleSummary) -> f64 {
    let sleep_s: f64;
    if summary.error.is_some() {
        sleep_s = 5.0 * 60.0;
    } else if {
        matches!(
            summary.state,
            Some(JobState::Done | JobState::Killed | JobState::Failed)
        )
    } {
        let queued = store.count_queued().await.unwrap_or(0);
        let inserted = summary.ingest.as_ref().map_or(0, |i| i.inserted);
        let enabled = store.count_enabled_repos().await.unwrap_or(0);
        sleep_s = if queued > 0 {
            0.0
        } else if inserted > 0 {
            5.0
        } else if enabled > 0 {
            60.0
        } else {
            15.0 * 60.0
        };
    } else if summary.denied.is_some() {
        if let Some(retry_at) = summary.retry_at.map(|v| v as f64) {
            if retry_at > 0.0 {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64();
                let until = retry_at / 1000.0 - now;
                sleep_s = (until + 30.0).clamp(60.0, 3600.0);
            } else {
                sleep_s = 30.0 * 60.0;
            }
        } else {
            sleep_s = 30.0 * 60.0;
        }
    } else {
        sleep_s = 15.0 * 60.0;
    }
    // PR sync cap (free reads only — never override budget denial backoff)
    if summary.sync.is_some() && summary.denied.is_none() {
        return sleep_s.min(PR_SYNC_INTERVAL_S);
    }
    sleep_s
}

/// Run the daemon: axum serve + scheduler loop + usage prober.
pub async fn run_daemon(cfg: Config) -> anyhow::Result<()> {
    let _lockfile = acquire_lockfile(&cfg.work_root)?;

    let store = Arc::new(Store::connect(&cfg.db_path).await?);
    anyhow::ensure!(
        cfg.backend_type == "omp-scavenge",
        "unknown backend_type: {:?}",
        cfg.backend_type
    );
    let backend: Arc<dyn Backend> = Arc::new(crate::backends::omp_scavenge::OmpScavengeBackend {
        cfg: cfg.clone(),
        ledger: store.clone() as Arc<dyn crate::backend::SpendLedger>,
        agent_db: crate::backends::omp_scavenge::default_agent_db(),
        prober: Arc::new(crate::backend::CmdProber),
    });

    let cycle_running = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(Notify::new());

    let state = AppState {
        store: store.clone(),
        config: Arc::new(cfg.clone()),
        backend: backend.clone(),
        scheduler: crate::server::SchedulerHandle {
            running: cycle_running.clone(),
            wake: wake.clone(),
        },
    };

    let port = cfg.serve_port;
    let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::from((
        std::net::Ipv4Addr::LOCALHOST,
        port,
    )))
    .await?;
    tracing::info!("daemon started: ui http://127.0.0.1:{port}/ -- scheduler loop live");

    // UI server task
    let ui_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router(state))
            .with_graceful_shutdown(async {
                tokio::signal::ctrl_c().await.ok();
            })
            .await;
    });

    // Usage prober task
    let prober_backend = backend.clone();
    let prober_handle = tokio::spawn(async move {
        // Immediate probe on startup (Python does keep_fresh() first, then sleeps)
        match prober_backend.keep_fresh().await {
            Ok(true) => tracing::debug!("usage probe: startup refresh"),
            Ok(false) => {}
            Err(e) => tracing::warn!("usage probe startup error: {e}"),
        }
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(USAGE_PROBE_TICK_S)).await;
            match prober_backend.keep_fresh().await {
                Ok(true) => tracing::debug!("usage probe: refreshed"),
                Ok(false) => {}
                Err(e) => tracing::warn!("usage probe error: {e}"),
            }
        }
    });

    // Scheduler loop
    let mut stop = false;
    while !stop {
        let sleep_s: f64;

        cycle_running.store(true, Ordering::SeqCst);
        let cycle_result: anyhow::Result<CycleSummary> = async {
            reconcile_and_log(&store).await?;
            let summary = crate::scheduler::run_cycle(&store, &cfg, &*backend, None).await;
            Ok(summary)
        }
        .await;
        cycle_running.store(false, Ordering::SeqCst);

        match cycle_result {
            Ok(summary) => {
                sleep_s = compute_sleep_s(&store, &summary).await;
                let (state_label, detail) = describe_cycle(&summary);
                let next_wake = crate::util::now_ms() + (sleep_s * 1000.0) as i64;
                let _ = store
                    .set_scheduler_state(&state_label, &detail, Some(next_wake))
                    .await;
                let summary_str = serde_json::to_string(&summary).unwrap_or_default();
                let summary_head: String = summary_str.chars().take(200).collect();
                tracing::info!("cycle: {summary_head} -> sleep {sleep_s:.0}s");
            }
            Err(e) => {
                tracing::error!("cycle crashed: {e:#}");
                sleep_s = 5.0 * 60.0;
                let next_wake = crate::util::now_ms() + (sleep_s * 1000.0) as i64;
                let _ = store
                    .set_scheduler_state(
                        "error",
                        &format!("daemon loop crashed: {e}")
                            .chars()
                            .take(200)
                            .collect::<String>(),
                        Some(next_wake),
                    )
                    .await;
            }
        }

        // Sleep, wake on signal or notify
        tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_secs_f64(sleep_s)) => {},
            () = wake.notified() => {
                tracing::debug!("daemon woken by notify");
            },
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received shutdown signal");
                stop = true;
            },
        }
    }

    prober_handle.abort();
    ui_handle.abort();
    tracing::info!("daemon stopped");
    Ok(())
}
