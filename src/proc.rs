//! Run a subprocess with a deadline. Every source this tool reads is a child
//! process (`gh`, `git`, `claude`), and none of them may stall a run past the
//! unit's own timeout.

use std::fmt;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use crate::text::prefix;

const POLL: Duration = Duration::from_millis(10);

pub struct Output {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub enum Failure {
    /// The program could not be started: usually not on PATH.
    Spawn(io::Error),
    TimedOut(Duration),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(err) => write!(f, "cannot start: {err}"),
            Self::TimedOut(limit) => write!(f, "timed out after {}s", limit.as_secs()),
        }
    }
}

/// Read a pipe to EOF on its own thread. Draining each pipe separately means a
/// child that fills one cannot deadlock against us reading the other.
fn drain(pipe: impl Read + Send + 'static) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut pipe = pipe;
        let mut buf = Vec::new();
        // A read error ends the stream; what arrived so far is what there is.
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

pub fn run(cmd: &mut Command, timeout: Duration) -> Result<Output, Failure> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(Failure::Spawn)?;
    let deadline = Instant::now() + timeout;
    let stdout = drain(child.stdout.take().expect("stdout is piped"));
    let stderr = drain(child.stderr.take().expect("stderr is piped"));

    // A grandchild can hold a pipe open after the child dies, so the reads are
    // bounded too. On timeout the reader threads are left to die with the process.
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let outputs = stdout
        .recv_timeout(remaining())
        .and_then(|out| Ok((out, stderr.recv_timeout(remaining())?)));
    let status = outputs.as_ref().ok().and_then(|_| {
        loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if !remaining().is_zero() => thread::sleep(POLL),
                _ => break None,
            }
        }
    });
    match (outputs, status) {
        (Ok((out, err)), Some(status)) => Ok(Output {
            success: status.success(),
            code: status.code(),
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
        }),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            Err(Failure::TimedOut(timeout))
        }
    }
}

/// What `git -C path args` printed. A timeout or a non-zero exit is a failed read:
/// it must never look like a repository with nothing in it.
pub fn git(path: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let out = run(Command::new("git").arg("-C").arg(path).args(args), timeout)
        .map_err(|failure| anyhow!("git {} in {}: {failure}", args.join(" "), path.display()))?;
    if !out.success {
        bail!(
            "git {} in {}: exit {:?}: {}",
            args.join(" "),
            path.display(),
            out.code,
            prefix(out.stderr.trim(), 200)
        );
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, timeout: Duration) -> Result<Output, Failure> {
        run(Command::new("/bin/sh").args(["-c", script]), timeout)
    }

    #[test]
    fn captures_both_streams_and_the_exit_code() {
        let out = sh("echo out; echo err >&2; exit 3", Duration::from_secs(10)).unwrap();
        assert_eq!(
            (
                out.stdout.as_str(),
                out.stderr.as_str(),
                out.code,
                out.success
            ),
            ("out\n", "err\n", Some(3), false)
        );
    }

    #[test]
    fn a_child_past_its_deadline_is_killed() {
        let started = Instant::now();
        let failure = sh("exec sleep 30", Duration::from_millis(100));
        assert!(matches!(failure, Err(Failure::TimedOut(_))));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_grandchild_holding_the_pipe_cannot_outlast_the_deadline() {
        let started = Instant::now();
        let failure = sh("sleep 30 & exit 0", Duration::from_millis(100));
        assert!(matches!(failure, Err(Failure::TimedOut(_))));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_missing_program_is_a_spawn_failure_not_a_timeout() {
        let failure = run(
            &mut Command::new("/nonexistent/program"),
            Duration::from_secs(1),
        );
        assert!(matches!(failure, Err(Failure::Spawn(_))));
    }
}
