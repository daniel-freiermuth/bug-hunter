//! CLI entry point: `hunter <serve|daemon> [--root <path>] [--port <port>]`.
//!
//! Hand-parsed args -- two subcommands and two flags do not justify a CLI
//! dependency. `daemon` takes an exclusive lock on <`work_root>/hunter.lock`;
//! `serve` does not, so a read-only UI can run beside a running daemon.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use hunter::config::Config;
use hunter::server::{AppState, router};
use hunter::store::Store;

const USAGE: &str = "usage: hunter <serve|daemon> [--root <path>] [--port <port>]

  serve          UI only, no scheduler loop
  daemon         UI + scheduler loop + usage prober (production mode)
  --root <path>  hunter/ project root to serve from (default: ../hunter)
  --port <port>  listen port (default: serve.port from config.json)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Serve,
    Daemon,
}

struct Cli {
    command: Command,
    root: PathBuf,
    port: Option<u16>,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut root = PathBuf::from("../hunter");
    let mut port = None;
    let mut command = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => {
                root = args
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| "--root requires a value".to_owned())?;
            }
            "--port" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--port requires a value".to_owned())?;
                port = Some(
                    value
                        .parse::<u16>()
                        .map_err(|_| format!("invalid port {value:?}"))?,
                );
            }
            "serve" if command.is_none() => command = Some(Command::Serve),
            "daemon" if command.is_none() => command = Some(Command::Daemon),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let command =
        command.ok_or_else(|| "missing subcommand (expected: serve or daemon)".to_owned())?;
    Ok(Cli {
        command,
        root,
        port,
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match parse_args(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(msg) => {
            eprintln!("error: {msg}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("fatal: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let cfg = Config::load(&cli.root)?;

    match cli.command {
        Command::Daemon => {
            let mut cfg = cfg;
            if let Some(port) = cli.port {
                cfg.serve_port = port;
            }
            hunter::daemon::run_daemon(cfg).await
        }
        Command::Serve => {
            let port = cli.port.unwrap_or(cfg.serve_port);
            let store = Arc::new(Store::connect(&cfg.db_path).await?);
            anyhow::ensure!(
                cfg.backend_type == "omp-scavenge",
                "unknown backend_type: {:?}",
                cfg.backend_type
            );
            let backend = Arc::new(hunter::backends::omp_scavenge::OmpScavengeBackend {
                cfg: cfg.clone(),
                ledger: store.clone() as Arc<dyn hunter::backend::SpendLedger>,
                agent_db: hunter::backends::omp_scavenge::default_agent_db(),
                prober: Arc::new(hunter::backend::CmdProber),
            });
            let state = AppState {
                store,
                config: Arc::new(cfg),
                backend,
                wake: Arc::new(tokio::sync::Notify::new()),
                py_base: String::new(),
            };
            let listener =
                tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
                    .await?;
            tracing::info!("hunter-rs serving on http://{}", listener.local_addr()?);
            axum::serve(listener, router(state))
                .with_graceful_shutdown(async {
                    if let Err(err) = tokio::signal::ctrl_c().await {
                        tracing::error!("failed to install ctrl-c handler: {err}");
                    }
                })
                .await?;
            Ok(())
        }
    }
}
