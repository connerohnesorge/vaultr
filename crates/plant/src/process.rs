//! Shared subprocess execution helpers for orchestration and maintenance.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// How a subprocess run ended. Distinguishes the cases `ok: false` collapses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RunEnd {
    Exited(Option<i32>),
    TimedOut,
    SpawnFailed,
    WaitFailed,
    OutputFailed,
}

#[derive(Debug, Default)]
pub struct CleanupDiagnostics {
    pub kill_error: Option<String>,
    pub reap_error: Option<String>,
}

pub struct RunResult {
    pub ok: bool,
    pub out: String,
    pub stderr: String,
    pub end: RunEnd,
    pub cleanup: Option<CleanupDiagnostics>,
}

impl RunResult {
    pub fn failure_detail(&self) -> String {
        let how = match self.end {
            RunEnd::Exited(Some(code)) => format!("exit {code}"),
            RunEnd::Exited(None) => "killed by signal".to_string(),
            RunEnd::TimedOut => "timed out".to_string(),
            RunEnd::SpawnFailed => "spawn failed".to_string(),
            RunEnd::WaitFailed => "wait failed".to_string(),
            RunEnd::OutputFailed => "output read failed".to_string(),
        };
        let mut diagnostics = Vec::new();
        let error: String = self.stderr.trim().chars().take(200).collect();
        if !error.is_empty() {
            diagnostics.push(error);
        }
        if let Some(cleanup) = &self.cleanup {
            if let Some(error) = &cleanup.kill_error {
                diagnostics.push(format!("kill failed: {error}"));
            }
            if let Some(error) = &cleanup.reap_error {
                diagnostics.push(format!("reap failed: {error}"));
            }
        }
        if diagnostics.is_empty() {
            how
        } else {
            format!("{how}: {}", diagnostics.join("; "))
        }
    }
}

/// PATH that works no matter who spawned Plant.
pub fn augmented_path() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut parts = Vec::new();
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            parts.push(directory.display().to_string());
        }
    }
    for directory in [".nix-profile/bin", ".local/bin", ".bun/bin"] {
        parts.push(format!("{home}/{directory}"));
    }
    parts.push("/opt/homebrew/bin".into());
    parts.push("/usr/local/bin".into());
    parts.push(std::env::var("PATH").unwrap_or_default());
    parts.join(":")
}

async fn read_all(reader: Option<impl AsyncRead + Unpin>) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if let Some(mut reader) = reader {
        reader.read_to_end(&mut bytes).await?;
    }
    Ok(bytes)
}

async fn kill_and_reap(child: &mut tokio::process::Child) -> CleanupDiagnostics {
    let kill_error = child.start_kill().err().map(|error| error.to_string());
    let reap_error = child.wait().await.err().map(|error| error.to_string());
    CleanupDiagnostics {
        kill_error,
        reap_error,
    }
}

enum CompletionError {
    Wait(std::io::Error),
    Stdout(std::io::Error),
    Stderr(std::io::Error),
}

/// Run a fully configured command under one deadline covering the direct child
/// and every pipe the caller requested. Inherited pipes are dropped before the
/// direct child is explicitly killed and reaped. Callers must await this future
/// to completion; external task abortion retains only `kill_on_drop` as a
/// cancellation backstop and cannot promise explicit reap.
pub async fn run_command(mut command: Command, timeout: Duration) -> RunResult {
    let deadline = tokio::time::Instant::now() + timeout;
    command.kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return RunResult {
                ok: false,
                out: String::new(),
                stderr: error.to_string(),
                end: RunEnd::SpawnFailed,
                cleanup: None,
            };
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let completed = {
        let joined = async {
            tokio::try_join!(
                async { child.wait().await.map_err(CompletionError::Wait) },
                async { read_all(stdout).await.map_err(CompletionError::Stdout) },
                async { read_all(stderr).await.map_err(CompletionError::Stderr) }
            )
        };
        tokio::time::timeout_at(deadline, joined).await
    };
    // The joined future and its optional drain handles are dropped before any
    // cleanup borrows the retained Child below.
    match completed {
        Ok(Ok((status, stdout, stderr))) => RunResult {
            ok: status.success(),
            out: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            end: RunEnd::Exited(status.code()),
            cleanup: None,
        },
        Ok(Err(error)) => {
            let cleanup = kill_and_reap(&mut child).await;
            let (end, error) = match error {
                CompletionError::Wait(error) => (RunEnd::WaitFailed, error.to_string()),
                CompletionError::Stdout(error) => {
                    (RunEnd::OutputFailed, format!("read stdout: {error}"))
                }
                CompletionError::Stderr(error) => {
                    (RunEnd::OutputFailed, format!("read stderr: {error}"))
                }
            };
            RunResult {
                ok: false,
                out: String::new(),
                stderr: error,
                end,
                cleanup: Some(cleanup),
            }
        }
        Err(_) => {
            let cleanup = kill_and_reap(&mut child).await;
            RunResult {
                ok: false,
                out: String::new(),
                stderr: String::new(),
                end: RunEnd::TimedOut,
                cleanup: Some(cleanup),
            }
        }
    }
}

pub async fn run(command: &[&str], timeout: Duration) -> RunResult {
    let mut configured = Command::new(command[0]);
    configured
        .args(&command[1..])
        .env("PATH", augmented_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_command(configured, timeout).await
}

pub async fn run30(command: &[&str]) -> RunResult {
    run(command, Duration::from_secs(30)).await
}

pub fn which(binary: &str) -> bool {
    augmented_path()
        .split(':')
        .any(|directory| Path::new(directory).join(binary).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_reaps_the_direct_child_before_a_late_marker() {
        let root =
            std::env::temp_dir().join(format!("plant-process-timeout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pid_path = root.join("pid");
        let marker_path = root.join("late-marker");
        let result = run(
            &[
                "sh",
                "-c",
                "printf '%s' $$ > \"$1\"; sleep 1; printf late > \"$2\"",
                "plant-process-test",
                pid_path.to_str().unwrap(),
                marker_path.to_str().unwrap(),
            ],
            Duration::from_millis(250),
        )
        .await;

        assert_eq!(result.end, RunEnd::TimedOut);
        let pid: libc::pid_t = std::fs::read_to_string(&pid_path).unwrap().parse().unwrap();
        // SAFETY: signal 0 only probes whether the explicitly reaped pid exists.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!marker_path.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inherited_pipe_cannot_extend_the_runner_deadline() {
        let started = std::time::Instant::now();
        let result = run(
            &[
                "sh",
                "-c",
                "sleep 2 >&1 2>&2 & exit 0",
                "plant-process-test",
            ],
            Duration::from_millis(50),
        )
        .await;

        assert_eq!(result.end, RunEnd::TimedOut);
        assert!(
            started.elapsed() < Duration::from_millis(750),
            "inherited pipe held the runner for {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preconfigured_file_stdio_drains_only_requested_stderr() {
        use std::fs::File;

        let root =
            std::env::temp_dir().join(format!("plant-process-stdio-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source");
        let frame = root.join("frame");
        std::fs::write(&source, b"descriptor input\n").unwrap();
        let mut command = Command::new("sh");
        command
            .args(["-c", "cat; printf compressor-warning >&2"])
            .stdin(Stdio::from(File::open(&source).unwrap()))
            .stdout(Stdio::from(File::create(&frame).unwrap()))
            .stderr(Stdio::piped());

        let result = run_command(command, Duration::from_secs(2)).await;

        assert_eq!(result.end, RunEnd::Exited(Some(0)));
        assert!(result.ok);
        assert!(
            result.out.is_empty(),
            "file stdout was unexpectedly captured"
        );
        assert_eq!(result.stderr, "compressor-warning");
        assert_eq!(std::fs::read(&frame).unwrap(), b"descriptor input\n");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn exit_75_and_spawn_failure_remain_typed() {
        let retry = run(&["sh", "-c", "exit 75"], Duration::from_secs(2)).await;
        assert_eq!(retry.end, RunEnd::Exited(Some(75)));
        assert!(!retry.ok);

        let missing = run(
            &["plant-command-that-does-not-exist"],
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(missing.end, RunEnd::SpawnFailed);
        assert!(missing.cleanup.is_none());
    }
}
