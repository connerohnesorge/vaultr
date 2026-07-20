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
        let err: String = self.stderr.trim().chars().take(200).collect();
        if err.is_empty() {
            how
        } else {
            format!("{how}: {err}")
        }
    }
}

/// PATH that works no matter who spawned Plant.
pub fn augmented_path() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut parts = vec![];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            parts.push(dir.display().to_string());
        }
    }
    for dir in [".nix-profile/bin", ".local/bin", ".bun/bin"] {
        parts.push(format!("{home}/{dir}"));
    }
    parts.push("/opt/homebrew/bin".into());
    parts.push("/usr/local/bin".into());
    parts.push(std::env::var("PATH").unwrap_or_default());
    parts.join(":")
}

pub async fn run(cmd: &[&str], timeout: Duration) -> RunResult {
    use tokio::io::AsyncReadExt;

    let mut command = tokio::process::Command::new(cmd[0]);
    command
        .args(&cmd[1..])
        .env("PATH", augmented_path())
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            return RunResult {
                ok: false,
                out: String::new(),
                stderr: e.to_string(),
                end: RunEnd::SpawnFailed,
            };
        }
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut out = Vec::new();
    let mut err = Vec::new();
    let completed = async {
        let (stdout, stderr, status) = tokio::join!(
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
            child.wait()
        );
        stdout?;
        stderr?;
        status
    };
    match tokio::time::timeout(timeout, completed).await {
        Ok(Ok(status)) => RunResult {
            ok: status.success(),
            out: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
            end: RunEnd::Exited(status.code()),
        },
        Ok(Err(e)) => RunResult {
            ok: false,
            out: String::new(),
            stderr: e.to_string(),
            end: RunEnd::SpawnFailed,
        },
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            RunResult {
                ok: false,
                out: String::new(),
                stderr: String::new(),
                end: RunEnd::TimedOut,
            }
        }
    }
}

pub async fn run30(cmd: &[&str]) -> RunResult {
    run(cmd, Duration::from_secs(30)).await
}

pub fn which(bin: &str) -> bool {
    augmented_path()
        .split(':')
        .any(|dir| Path::new(dir).join(bin).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn timeout_kills_and_reaps_direct_child_before_returning() {
        let root = std::env::temp_dir().join(format!(
            "plant-run-timeout-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let pid_path = root.join("pid");
        let marker = root.join("late-marker");
        let script = format!(
            "echo $$ > '{}'; sleep 1; echo late > '{}'",
            pid_path.display(),
            marker.display()
        );

        let result = run(&["/bin/sh", "-c", &script], Duration::from_millis(200)).await;
        assert_eq!(result.end, RunEnd::TimedOut);
        let pid: i32 = std::fs::read_to_string(&pid_path)
            .expect("child published its pid before timeout")
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH),
            "timed-out direct child must already be reaped"
        );
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!marker.exists(), "delayed child effect occurred");
        let _ = std::fs::remove_dir_all(root);
    }
}
