//! Small shared utilities (port of hunter/util.py where still needed).

use std::process::{Command, Stdio};
use std::time::Duration;

/// Epoch milliseconds (types.py `now_ms`).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Run a command, merged stdout+stderr, never fails (util.py:9-40):
/// timeout -> (124, msg); spawn error -> (127, msg). Blocking — call via
/// `spawn_blocking` from async contexts.
pub fn run_cmd(argv: &[&str], timeout_s: u64) -> (i32, String) {
    let Some((prog, rest)) = argv.split_first() else {
        return (127, "empty argv".to_owned());
    };
    let child = Command::new(prog)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (127, e.to_string()),
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_s);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                use std::io::Read;
                let mut out = String::new();
                if let Some(mut so) = child.stdout.take() {
                    let _ = so.read_to_string(&mut out);
                }
                if let Some(mut se) = child.stderr.take() {
                    let _ = se.read_to_string(&mut out);
                }
                return (status.code().unwrap_or(-1), out);
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (124, format!("timeout after {timeout_s}s"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return (127, e.to_string()),
        }
    }
}
