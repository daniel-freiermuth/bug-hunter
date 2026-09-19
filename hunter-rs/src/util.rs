//! Small shared utilities (port of hunter/util.py where still needed).

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

/// Epoch milliseconds (types.py `now_ms`).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Read a child pipe to EOF on its own thread. Both pipes have to be
/// drained while the child runs — a child that fills a pipe buffer blocks
/// forever if we only read after it exits (util.py got this for free from
/// `communicate()`).
///
/// Bytes, not `String`: `read_to_string` aborts on the first invalid UTF-8
/// sequence AND leaves the buffer empty, so one stray byte from a git diff
/// or a worker would discard the whole stream and stop draining it —
/// reinstating the deadlock this exists to prevent.
pub fn drain_pipe<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    })
}

/// Join a pair of pipe readers, stdout first — the merged output util.py
/// produced with `stderr=STDOUT`. Decoded lossily once, after both streams
/// reach EOF. A reader that panicked contributes nothing.
pub fn join_pipes(so: JoinHandle<Vec<u8>>, se: JoinHandle<Vec<u8>>) -> String {
    let mut bytes = so.join().unwrap_or_default();
    bytes.extend_from_slice(&se.join().unwrap_or_default());
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Last `max_bytes` of `s`, snapped forward to a char boundary.
///
/// Command output and worker logs are arbitrary UTF-8, so indexing a tail
/// by byte offset panics whenever the cut lands inside a codepoint. The
/// result is always a suffix and never longer than `max_bytes`.
pub fn tail(s: &str, max_bytes: usize) -> &str {
    let mut start = s.len().saturating_sub(max_bytes);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Run a command, merged stdout+stderr, never fails (util.py:9-40):
/// timeout -> (124, msg + whatever was captured); spawn error -> (127, msg).
/// Blocking — call via `spawn_blocking` from async contexts.
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
    // Readers start now, not after the wait: see `drain_pipe`. Every exit
    // path below reaps the child first, so the joins see a closed pipe.
    let so = drain_pipe(child.stdout.take());
    let se = drain_pipe(child.stderr.take());
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_s);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return (status.code().unwrap_or(-1), join_pipes(so, se));
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let out = join_pipes(so, se);
                    return (124, format!("timeout after {timeout_s}s\n{out}"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let out = join_pipes(so, se);
                return (127, format!("{e}\n{out}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_cmd_outlives_a_full_pipe_buffer() {
        // 300 KB per stream, far past the 64 KiB pipe buffer. Read only
        // after the wait, the child blocks on write, never exits, and this
        // comes back as (124, "timeout after 30s") instead.
        let (rc, out) = run_cmd(
            &[
                "sh",
                "-c",
                "yes abcdefghij | head -c 300000; yes klmnopqrst | head -c 300000 >&2",
            ],
            30,
        );
        assert_eq!(rc, 0);
        assert_eq!(out.len(), 600_000);
    }

    /// `read_to_string` would abort on the first invalid byte and leave the
    /// buffer EMPTY -- losing the whole stream and stopping the drain.
    #[test]
    fn test_run_cmd_survives_invalid_utf8() {
        let (rc, out) = run_cmd(&["sh", "-c", "printf 'before\\377after'"], 30);
        assert_eq!(rc, 0);
        assert!(
            out.contains("before"),
            "lost output before the bad byte: {out:?}"
        );
        assert!(
            out.contains("after"),
            "stopped draining at the bad byte: {out:?}"
        );
        assert!(
            out.contains('\u{fffd}'),
            "bad byte should decode lossily: {out:?}"
        );
    }

    // -- tail -------------------------------------------------------------
    //
    // `tail` truncates command output and worker logs at ~20 call sites,
    // all of them on bytes that came from a subprocess. Indexing the tail
    // by byte offset panicked whenever the cut landed inside a codepoint,
    // which a diff containing one non-ASCII character is enough to trigger.

    #[test]
    fn test_tail_shorter_than_limit_is_whole() {
        assert_eq!(tail("abc", 10), "abc");
        assert_eq!(tail("", 10), "");
    }

    #[test]
    fn test_tail_exactly_limit_is_whole() {
        assert_eq!(tail("abcdef", 6), "abcdef");
    }

    #[test]
    fn test_tail_longer_than_limit_keeps_the_end() {
        let s = "abcdefghij";
        let got = tail(s, 3);
        // The end is the interesting part of a log: a prefix would show the
        // startup banner and drop the error.
        assert!(s.ends_with(got), "{got:?} is not a suffix of {s:?}");
        assert_eq!(got, "hij");
    }

    /// The panic this function exists to prevent. `€` is 3 bytes, the limit
    /// is 7, so the raw cut at `len - 7` lands one byte into a codepoint --
    /// a limit divisible by the char width would pass even with byte
    /// slicing restored.
    #[test]
    fn test_tail_does_not_split_a_codepoint() {
        let s = "€€€€€"; // 15 bytes, boundaries at 0/3/6/9/12/15
        assert_eq!(s.len(), 15);
        let got = tail(s, 7);
        assert!(s.ends_with(got), "{got:?} is not a suffix of {s:?}");
        assert!(got.len() <= 7, "{} bytes exceeds the limit", got.len());
        // Snapped forward from byte 8 to byte 9, so two whole chars.
        assert_eq!(got, "€€");
    }

    /// Same cut, but with the split codepoint at the very front of what
    /// survives and ASCII behind it -- the shape a truncated stack trace has.
    #[test]
    fn test_tail_snaps_forward_past_a_partial_char() {
        let s = "aaa€bbb"; // 9 bytes: cut at 9-5=4 is inside the `€`
        let got = tail(s, 5);
        assert!(s.ends_with(got), "{got:?} is not a suffix of {s:?}");
        assert!(got.len() <= 5, "{} bytes exceeds the limit", got.len());
        assert_eq!(got, "bbb");
    }

    #[test]
    fn test_tail_zero_limit_is_empty() {
        assert_eq!(tail("abcdef", 0), "");
        assert_eq!(tail("€", 0), "");
        assert_eq!(tail("", 0), "");
    }
}
