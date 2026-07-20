//! Shared subprocess execution helpers for orchestration and maintenance.

use std::path::Path;
use std::time::Duration;

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

pub async fn run(command: &[&str], timeout: Duration) -> RunResult {
    let process = tokio::process::Command::new(command[0])
        .args(&command[1..])
        .env("PATH", augmented_path())
        .output();
    match tokio::time::timeout(timeout, process).await {
        Ok(Ok(output)) => RunResult {
            ok: output.status.success(),
            out: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            end: RunEnd::Exited(output.status.code()),
        },
        Ok(Err(error)) => RunResult {
            ok: false,
            out: String::new(),
            stderr: error.to_string(),
            end: RunEnd::SpawnFailed,
        },
        Err(_) => RunResult {
            ok: false,
            out: String::new(),
            stderr: String::new(),
            end: RunEnd::TimedOut,
        },
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
