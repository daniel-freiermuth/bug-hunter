//! CLI entry point: `hunter [--root <path>] [--port <port>]`.
//!
//! Hand-parsed args -- two flags do not justify a CLI dependency. The
//! process takes an exclusive lock on <`work_root>/hunter.lock` and owns
//! the database schema: it runs the embedded migrations on startup.

use std::path::PathBuf;
use std::process::ExitCode;

use hunter::config::Config;

const USAGE: &str = "usage: hunter [--root <path>] [--port <port>]

  --root <path>  hunter/ project root to serve from (default: ../hunter)
  --port <port>  listen port (default: serve.port from config.json)";

struct Cli {
    root: PathBuf,
    port: Option<u16>,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut root = PathBuf::from("../hunter");
    let mut port = None;
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
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Cli { root, port })
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
    let mut cfg = Config::load(&cli.root)?;
    if let Some(port) = cli.port {
        cfg.serve_port = port;
    }
    hunter::daemon::run_daemon(cfg).await
}
