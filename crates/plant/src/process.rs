use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum RunEnd {
    Exited(Option<i32>),
    TimedOut,
    SpawnFailed,
    WaitFailed,
}

pub(crate) struct RunResult {
    pub(crate) ok: bool,
    pub(crate) out: String,
    pub(crate) stderr: String,
    pub(crate) end: RunEnd,
}

impl RunResult {
    pub(crate) fn failure_detail(&self) -> String {
        let how = match self.end {
            RunEnd::Exited(Some(code)) => format!("exit {code}"),
            RunEnd::Exited(None) => "killed by signal".to_string(),
            RunEnd::TimedOut => "timed out".to_string(),
            RunEnd::SpawnFailed => "spawn failed".to_string(),
            RunEnd::WaitFailed => "wait failed".to_string(),
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
pub(crate) fn augmented_path() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut parts = vec![];
    let executable_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    if let Some(dir) = executable_dir {
        parts.push(dir.display().to_string());
    }
    for dir in [".nix-profile/bin", ".local/bin", ".bun/bin"] {
        parts.push(format!("{home}/{dir}"));
    }
    parts.push("/opt/homebrew/bin".into());
    parts.push("/usr/local/bin".into());
    parts.push(std::env::var("PATH").unwrap_or_default());
    parts.join(":")
}

fn output_task<T>(stream: Option<T>) -> Option<tokio::task::JoinHandle<std::io::Result<Vec<u8>>>>
where
    T: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    stream.map(|mut stream| {
        tokio::spawn(async move {
            let mut bytes = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut bytes)
                .await
                .map(|_| bytes)
        })
    })
}

async fn output(
    task: &mut Option<tokio::task::JoinHandle<std::io::Result<Vec<u8>>>>,
) -> (Vec<u8>, Option<String>) {
    let Some(task) = task else {
        return (Vec::new(), None);
    };
    match task.await {
        Ok(Ok(bytes)) => (bytes, None),
        Ok(Err(error)) => (Vec::new(), Some(error.to_string())),
        Err(error) => (Vec::new(), Some(error.to_string())),
    }
}

fn abort_output(task: &Option<tokio::task::JoinHandle<std::io::Result<Vec<u8>>>>) {
    if let Some(task) = task {
        task.abort();
    }
}

async fn kill_and_reap(child: &mut tokio::process::Child) -> String {
    let kill = child
        .start_kill()
        .err()
        .map(|error| format!("kill: {error}"));
    let reap = child
        .wait()
        .await
        .err()
        .map(|error| format!("reap: {error}"));
    [kill, reap]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ")
}

fn failed(end: RunEnd, stderr: String) -> RunResult {
    RunResult {
        ok: false,
        out: String::new(),
        stderr,
        end,
    }
}

/// Run an already-configured command. Piped streams are drained; inherited or
/// null streams stay as configured by the caller. Every post-spawn failure path
/// kills and reaps the direct child before returning.
pub(crate) async fn run_command(
    command: &mut tokio::process::Command,
    timeout: Duration,
) -> RunResult {
    command.kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return failed(RunEnd::SpawnFailed, error.to_string()),
    };
    let mut stdout_task = output_task(child.stdout.take());
    let mut stderr_task = output_task(child.stderr.take());
    let deadline = tokio::time::Instant::now() + timeout;

    let end = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(status)) => RunEnd::Exited(status.code()),
        Ok(Err(error)) => {
            abort_output(&stdout_task);
            abort_output(&stderr_task);
            let cleanup = kill_and_reap(&mut child).await;
            let detail = [format!("wait: {error}"), cleanup]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("; ");
            return failed(RunEnd::WaitFailed, detail);
        }
        Err(_) => {
            abort_output(&stdout_task);
            abort_output(&stderr_task);
            let cleanup = kill_and_reap(&mut child).await;
            return failed(RunEnd::TimedOut, cleanup);
        }
    };

    let drains = async { tokio::join!(output(&mut stdout_task), output(&mut stderr_task)) };
    let ((out, out_error), (stderr, stderr_error)) =
        match tokio::time::timeout_at(deadline, drains).await {
            Ok(output) => output,
            Err(_) => {
                abort_output(&stdout_task);
                abort_output(&stderr_task);
                return failed(RunEnd::TimedOut, "output drain timed out".to_string());
            }
        };
    let diagnostics = [out_error, stderr_error]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ");
    let mut stderr = String::from_utf8_lossy(&stderr).into_owned();
    if !diagnostics.is_empty() && !stderr.is_empty() {
        stderr.push_str("; ");
    }
    if !diagnostics.is_empty() {
        stderr.push_str(&diagnostics);
    }
    let end = if diagnostics.is_empty() {
        end
    } else {
        RunEnd::WaitFailed
    };
    RunResult {
        ok: matches!(end, RunEnd::Exited(Some(0))),
        out: String::from_utf8_lossy(&out).into_owned(),
        stderr,
        end,
    }
}

pub(crate) async fn run(cmd: &[&str], timeout: Duration) -> RunResult {
    let mut command = tokio::process::Command::new(cmd[0]);
    command
        .args(&cmd[1..])
        .env("PATH", augmented_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_command(&mut command, timeout).await
}

pub(crate) async fn run30(cmd: &[&str]) -> RunResult {
    run(cmd, Duration::from_secs(30)).await
}

/// Spawn a child whose lifetime does not belong to the caller's Tokio task.
///
/// The caller must close or redirect all standard streams before calling this
/// function. The reaper task prevents ordinary workers from becoming zombies.
/// A worker remains alive if the scheduler task or the proxy daemon exits.
pub(crate) fn spawn_detached(command: &mut tokio::process::Command) -> std::io::Result<()> {
    command.kill_on_drop(false);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

pub(crate) fn which(bin: &str) -> bool {
    augmented_path()
        .split(':')
        .any(|dir| Path::new(dir).join(bin).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn detached_child_survives_owner_task_cancellation() {
        let root = std::env::temp_dir().join(format!(
            "plant-detached-worker-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let marker = root.join("completed");
        let marker_text = marker.display().to_string();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();

        let owner = tokio::spawn(async move {
            let mut command = tokio::process::Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    &format!("/bin/sleep 0.2; printf done > '{}'", marker_text),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            spawn_detached(&mut command).unwrap();
            started_tx.send(()).unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        started_rx.await.unwrap();
        owner.abort();
        let _ = owner.await;

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "done");
        std::fs::remove_dir_all(root).unwrap();
    }

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
        assert!(!result.ok);
        assert_eq!(result.failure_detail(), "timed out");
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

    #[tokio::test]
    async fn ordinary_outcomes_keep_their_output_and_exit_classification() {
        let success = run(
            &["/bin/sh", "-c", "printf success; printf warning >&2"],
            Duration::from_secs(1),
        )
        .await;
        assert!(success.ok);
        assert_eq!(success.end, RunEnd::Exited(Some(0)));
        assert_eq!(success.out, "success");
        assert_eq!(success.stderr, "warning");

        let nonzero = run(
            &[
                "/bin/sh",
                "-c",
                "printf partial; printf failed >&2; exit 23",
            ],
            Duration::from_secs(1),
        )
        .await;
        assert!(!nonzero.ok);
        assert_eq!(nonzero.end, RunEnd::Exited(Some(23)));
        assert_eq!(nonzero.out, "partial");
        assert_eq!(nonzero.failure_detail(), "exit 23: failed");

        let spawn_error = run(&["/definitely/not/a/plant-command"], Duration::from_secs(1)).await;
        assert!(!spawn_error.ok);
        assert_eq!(spawn_error.end, RunEnd::SpawnFailed);
        assert!(spawn_error.out.is_empty());
        assert!(spawn_error.failure_detail().starts_with("spawn failed:"));
    }

    #[tokio::test]
    async fn descendant_inheriting_a_pipe_cannot_extend_the_deadline() {
        let root =
            std::env::temp_dir().join(format!("plant-process-pipe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let pid_path = root.join("descendant.pid");
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!("sleep 10 & echo $! > '{}'", pid_path.display()))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let started = Instant::now();
        let result = run_command(&mut command, Duration::from_millis(100)).await;
        assert_eq!(result.end, RunEnd::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "an inherited pipe must not keep the runner alive"
        );

        let descendant_pid = std::fs::read_to_string(&pid_path)
            .ok()
            .and_then(|pid| pid.trim().parse::<i32>().ok());
        if let Some(pid) = descendant_pid {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
