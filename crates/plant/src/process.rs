//! Shared subprocess execution helpers for orchestration and maintenance.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

/// How a subprocess run ended. Distinguishes the cases `ok: false` collapses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RunEnd {
    Exited(Option<i32>),
    TimedOut,
    SpawnFailed,
}

pub struct RunResult {
    pub ok: bool,
    pub out: String,
    pub stderr: String,
    pub end: RunEnd,
}

impl RunResult {
    pub fn failure_detail(&self) -> String {
        let how = match self.end {
            RunEnd::Exited(Some(code)) => format!("exit {code}"),
            RunEnd::Exited(None) => "killed by signal".to_string(),
            RunEnd::TimedOut => "timed out".to_string(),
            RunEnd::SpawnFailed => "spawn failed".to_string(),
        };
        let error: String = self.stderr.trim().chars().take(200).collect();
        if error.is_empty() {
            how
        } else {
            format!("{how}: {error}")
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

async fn read_all(mut reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

async fn kill_and_reap(child: &mut tokio::process::Child) -> Option<String> {
    let kill = child.start_kill().err();
    let wait = child.wait().await.err();
    match (kill, wait) {
        (None, None) => None,
        (Some(kill), None) if kill.kind() == std::io::ErrorKind::InvalidInput => None,
        (Some(kill), None) => Some(format!("kill failed: {kill}")),
        (None, Some(wait)) => Some(format!("reap failed: {wait}")),
        (Some(kill), Some(wait)) => Some(format!("kill failed: {kill}; reap failed: {wait}")),
    }
}

pub async fn run(command: &[&str], timeout: Duration) -> RunResult {
    let mut child = match tokio::process::Command::new(command[0])
        .args(&command[1..])
        .env("PATH", augmented_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return RunResult {
                ok: false,
                out: String::new(),
                stderr: error.to_string(),
                end: RunEnd::SpawnFailed,
            };
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let completed = tokio::time::timeout(timeout, async {
        tokio::try_join!(child.wait(), read_all(stdout), read_all(stderr))
    })
    .await;
    match completed {
        Ok(Ok((status, stdout, stderr))) => RunResult {
            ok: status.success(),
            out: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            end: RunEnd::Exited(status.code()),
        },
        Ok(Err(error)) => {
            let cleanup = kill_and_reap(&mut child).await;
            RunResult {
                ok: false,
                out: String::new(),
                stderr: match cleanup {
                    Some(cleanup) => format!("{error}; {cleanup}"),
                    None => error.to_string(),
                },
                end: RunEnd::SpawnFailed,
            }
        }
        Err(_) => {
            let cleanup = kill_and_reap(&mut child).await;
            RunResult {
                ok: false,
                out: String::new(),
                stderr: cleanup.unwrap_or_default(),
                end: RunEnd::TimedOut,
            }
        }
    }
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
}
