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

async fn output(
    task: &mut tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
) -> (Vec<u8>, Option<String>) {
    match task.await {
        Ok(Ok(bytes)) => (bytes, None),
        Ok(Err(error)) => (Vec::new(), Some(error.to_string())),
        Err(error) => (Vec::new(), Some(error.to_string())),
    }
}

async fn kill_and_reap(child: &mut tokio::process::Child) -> String {
    let kill = child
        .start_kill()
        .err()
        .map(|error| format!("kill: {error}"));
    let reap = match tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
        Ok(Ok(_)) => None,
        Ok(Err(error)) => Some(format!("reap: {error}")),
        Err(_) => Some("reap timed out".to_string()),
    };
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

/// Run a configured command with piped output. Every path after a successful
/// spawn gives the direct child and both output drains one shared deadline.
/// Timeout and wait-error paths abort the drains, then explicitly kill and wait
/// for the child so no job or maintenance child is left unreaped.
pub(crate) async fn run_command(
    command: &mut tokio::process::Command,
    timeout: Duration,
) -> RunResult {
    command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
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
    let stdout = child.stdout.take().expect("command stdout is piped");
    let stderr = child.stderr.take().expect("command stderr is piped");
    let mut stdout_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        let mut stream = stdout;
        tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut bytes)
            .await
            .map(|_| bytes)
    });
    let mut stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        let mut stream = stderr;
        tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut bytes)
            .await
            .map(|_| bytes)
    });

    let deadline = tokio::time::Instant::now() + timeout;
    let end = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(status)) => RunEnd::Exited(status.code()),
        Ok(Err(error)) => {
            stdout_task.abort();
            stderr_task.abort();
            let cleanup = kill_and_reap(&mut child).await;
            let detail = [format!("wait: {error}"), cleanup]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("; ");
            return failed(RunEnd::WaitFailed, detail);
        }
        Err(_) => {
            stdout_task.abort();
            stderr_task.abort();
            let cleanup = kill_and_reap(&mut child).await;
            return failed(RunEnd::TimedOut, cleanup);
        }
    };

    let drains = async { tokio::join!(output(&mut stdout_task), output(&mut stderr_task)) };
    let ((out, out_error), (stderr, stderr_error)) =
        match tokio::time::timeout_at(deadline, drains).await {
            Ok(output) => output,
            Err(_) => {
                stdout_task.abort();
                stderr_task.abort();
                let cleanup = kill_and_reap(&mut child).await;
                return failed(RunEnd::TimedOut, cleanup);
            }
        };
    let diagnostics = [out_error, stderr_error]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ");
    let mut stderr = String::from_utf8_lossy(&stderr).into_owned();
    if !diagnostics.is_empty() {
        if !stderr.is_empty() {
            stderr.push_str("; ");
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn descendant_inheriting_a_pipe_cannot_extend_the_deadline() {
        let root =
            std::env::temp_dir().join(format!("plant-process-pipe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let pid_path = root.join("descendant.pid");
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!("sleep 10 & echo $! > '{}'", pid_path.display()));

        let started = Instant::now();
        let result = run_command(&mut command, Duration::from_millis(100)).await;
        assert_eq!(result.end, RunEnd::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "an inherited pipe must not keep the runner alive"
        );

        if let Ok(pid) = std::fs::read_to_string(&pid_path) {
            if let Ok(pid) = pid.trim().parse::<i32>() {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
