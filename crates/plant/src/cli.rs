use crate::credentials::ReconcileArgs;
use crate::domain::Harness;
use crate::herdr::WorkspaceCleanup;
use crate::jobs;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Daemon,
    ServerStop,
    SelfTest,
    SessionsEligible(EligibleArgs),
    SessionsCoverage(String),
    SessionsStuck(Duration),
    CompressOnce(Duration),
    JobsRun(String),
    JobsUnblock(String),
    JobsWorker(ScheduledWorkerArgs),
    AgentRun(AgentRunArgs),
    CredentialsReconcile(ReconcileArgs),
    /// `credentials reconcile --help` exists so a guest can cheaply probe
    /// whether the plant it shipped with has the reconciler at all. The image
    /// degrades loudly on absence rather than crashlooping, and that probe is
    /// how it tells.
    CredentialsHelp,
}

#[derive(Debug, PartialEq, Eq)]
pub struct EligibleArgs {
    pub learner: Harness,
    pub idle: Duration,
    pub max: usize,
    pub claim: Option<Duration>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct AgentRunArgs {
    pub cli: Harness,
    pub model: Option<String>,
    pub args: Option<String>,
    pub label: Option<String>,
    pub cleanup: WorkspaceCleanup,
    pub timeout: Duration,
    pub cwd: Option<String>,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ScheduledWorkerArgs {
    pub name: String,
    pub path: PathBuf,
    pub every: Duration,
    pub capacity: usize,
    pub timeout: Duration,
}

pub fn parse_command(argv: &[String]) -> Result<Command, String> {
    let args = argv.get(1..).unwrap_or_default();
    match args {
        [] => Ok(Command::Daemon),
        [group, action] if group == "server" && action == "stop" => Ok(Command::ServerStop),
        [flag] if flag == "--self-test" => Ok(Command::SelfTest),
        [group, action, sid] if group == "sessions" && action == "coverage" && !sid.is_empty() => {
            Ok(Command::SessionsCoverage(sid.clone()))
        }
        [group, action] if group == "sessions" && action == "stuck" => {
            Ok(Command::SessionsStuck(Duration::from_secs(24 * 3600)))
        }
        [group, action, flag, age]
            if group == "sessions" && action == "stuck" && flag == "--age" =>
        {
            jobs::parse_duration(age)
                .map(Command::SessionsStuck)
                .ok_or_else(|| "sessions stuck: invalid --age duration".to_string())
        }
        [group, action] if group == "compress" && action == "once" => {
            Ok(Command::CompressOnce(Duration::from_secs(3600)))
        }
        [group, action, flag, idle]
            if group == "compress" && action == "once" && flag == "--idle" =>
        {
            jobs::parse_duration(idle)
                .map(Command::CompressOnce)
                .ok_or_else(|| "compress once: invalid --idle duration".to_string())
        }
        [group, action, name] if group == "jobs" && action == "run" && !name.is_empty() => {
            Ok(Command::JobsRun(name.clone()))
        }
        [group, action, name] if group == "jobs" && action == "unblock" && !name.is_empty() => {
            Ok(Command::JobsUnblock(name.clone()))
        }
        [group, action, rest @ ..] if group == "jobs" && action == "worker" => {
            parse_worker(rest).map(Command::JobsWorker)
        }
        [group, action, rest @ ..] if group == "sessions" && action == "eligible" => {
            parse_eligible(rest).map(Command::SessionsEligible)
        }
        [group, action, rest @ ..] if group == "agent" && action == "run" => {
            parse_agent(rest).map(Command::AgentRun)
        }
        [group, action, rest @ ..] if group == "credentials" && action == "reconcile" => {
            if rest.iter().any(|arg| arg == "--help") {
                return Ok(Command::CredentialsHelp);
            }
            parse_reconcile(rest).map(Command::CredentialsReconcile)
        }
        _ => Err("unrecognized command".to_string()),
    }
}

fn parse_eligible(args: &[String]) -> Result<EligibleArgs, String> {
    let mut parsed = EligibleArgs {
        learner: Harness::ClaudeCode,
        idle: Duration::from_secs(3600),
        max: 10,
        claim: None,
    };
    let mut seen = HashSet::new();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        if !seen.insert(flag) {
            return Err(format!("duplicate sessions eligible flag {flag}"));
        }
        match flag {
            "--learner" => {
                let value = args
                    .get(i + 1)
                    .ok_or("sessions eligible: --learner requires claude|codex")?;
                parsed.learner = Harness::parse_ledger_label(value)
                    .ok_or("sessions eligible: --learner requires claude|codex")?;
                i += 2;
            }
            "--idle" => {
                let value = args
                    .get(i + 1)
                    .ok_or("sessions eligible: --idle requires a duration")?;
                parsed.idle = jobs::parse_duration(value)
                    .ok_or("sessions eligible: invalid --idle duration")?;
                i += 2;
            }
            "--max" => {
                let value = args
                    .get(i + 1)
                    .ok_or("sessions eligible: --max requires an integer")?;
                parsed.max = value
                    .parse()
                    .map_err(|_| "sessions eligible: invalid --max")?;
                i += 2;
            }
            "--claim" => match args.get(i + 1).map(String::as_str) {
                Some(value) if !value.starts_with("--") => {
                    parsed.claim = Some(
                        jobs::parse_duration(value)
                            .ok_or("sessions eligible: invalid --claim duration")?,
                    );
                    i += 2;
                }
                _ => {
                    parsed.claim = Some(Duration::from_secs(50 * 60));
                    i += 1;
                }
            },
            _ => return Err(format!("unknown sessions eligible flag {flag}")),
        }
    }
    Ok(parsed)
}

fn parse_agent(args: &[String]) -> Result<AgentRunArgs, String> {
    let mut cli = None;
    let mut model = None;
    let mut extra_args = None;
    let mut label = None;
    let mut cwd = None;
    let mut idempotency_key = None;
    let mut cleanup = WorkspaceCleanup::Never;
    let mut timeout = Duration::from_secs(45 * 60);
    let mut seen = HashSet::new();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        if !seen.insert(flag) {
            return Err(format!("duplicate agent run flag {flag}"));
        }
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("agent run: {flag} requires a value"))?;
        match flag {
            "--cli" => {
                cli = Some(
                    Harness::parse_cli_label(value)
                        .ok_or("agent run: --cli requires claude|codex")?,
                );
            }
            "--model" => model = Some(value.clone()),
            "--args" => extra_args = Some(value.clone()),
            "--label" => label = Some(value.clone()),
            "--cwd" => cwd = Some(value.clone()),
            "--idempotency-key" => idempotency_key = Some(value.clone()),
            "--timeout" => {
                timeout =
                    jobs::parse_duration(value).ok_or("agent run: invalid --timeout duration")?;
            }
            "--cleanup" => {
                cleanup = match value.as_str() {
                    "always" => WorkspaceCleanup::Always,
                    "on-success" => WorkspaceCleanup::OnSuccess,
                    "never" => WorkspaceCleanup::Never,
                    _ => {
                        return Err(
                            "agent run: --cleanup requires always|on-success|never".to_string()
                        );
                    }
                };
            }
            _ => return Err(format!("unknown agent run flag {flag}")),
        }
        i += 2;
    }
    Ok(AgentRunArgs {
        cli: cli.ok_or("agent run: --cli claude|codex is required")?,
        model,
        args: extra_args,
        label,
        cleanup,
        timeout,
        cwd,
        idempotency_key,
    })
}

fn parse_worker(args: &[String]) -> Result<ScheduledWorkerArgs, String> {
    let [name, path, every, capacity, timeout] = args else {
        return Err(
            "jobs worker: expected <name> <path> <every-seconds> <capacity> <timeout-seconds>"
                .to_string(),
        );
    };
    if name.is_empty() || path.is_empty() {
        return Err("jobs worker: name and path are required".to_string());
    }
    let every = every
        .parse::<u64>()
        .map(Duration::from_secs)
        .map_err(|_| "jobs worker: invalid every-seconds".to_string())?;
    if every.is_zero() {
        return Err("jobs worker: every-seconds must be positive".to_string());
    }
    let capacity = capacity
        .parse::<usize>()
        .map_err(|_| "jobs worker: invalid capacity".to_string())?;
    if capacity == 0 {
        return Err("jobs worker: capacity must be positive".to_string());
    }
    let timeout = timeout
        .parse::<u64>()
        .map(Duration::from_secs)
        .map_err(|_| "jobs worker: invalid timeout-seconds".to_string())?;
    Ok(ScheduledWorkerArgs {
        name: name.clone(),
        path: PathBuf::from(path),
        every,
        capacity,
        timeout,
    })
}

fn parse_reconcile(args: &[String]) -> Result<ReconcileArgs, String> {
    let mut parsed = ReconcileArgs::with_defaults();
    let mut seen = HashSet::new();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        if !seen.insert(flag) {
            return Err(format!("duplicate credentials reconcile flag {flag}"));
        }
        match flag {
            "--once" => {
                parsed.once = true;
                i += 1;
            }
            "--interval" => {
                let value = args
                    .get(i + 1)
                    .ok_or("credentials reconcile: --interval requires a duration")?;
                parsed.interval = jobs::parse_duration(value)
                    .ok_or("credentials reconcile: invalid --interval duration")?;
                i += 2;
            }
            "--source" => {
                let value = args
                    .get(i + 1)
                    .ok_or("credentials reconcile: --source requires a path")?;
                parsed.source = PathBuf::from(value);
                i += 2;
            }
            _ => return Err(format!("unknown credentials reconcile flag {flag}")),
        }
    }
    if parsed.once && seen.contains("--interval") {
        return Err("credentials reconcile: --once and --interval are exclusive".to_string());
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("plant")
            .chain(args.iter().copied())
            .map(String::from)
            .collect()
    }

    #[test]
    fn parses_bare_and_typed_eligible_forms() {
        assert_eq!(parse_command(&argv(&[])), Ok(Command::Daemon));
        assert_eq!(
            parse_command(&argv(&["server", "stop"])),
            Ok(Command::ServerStop)
        );
        assert_eq!(
            parse_command(&argv(&[
                "sessions",
                "eligible",
                "--learner",
                "codex",
                "--idle",
                "2h",
                "--max",
                "3",
                "--claim",
                "10m",
            ])),
            Ok(Command::SessionsEligible(EligibleArgs {
                learner: Harness::Codex,
                idle: Duration::from_secs(2 * 3600),
                max: 3,
                claim: Some(Duration::from_secs(10 * 60)),
            }))
        );
        assert_eq!(
            parse_command(&argv(&["sessions", "eligible", "--claim"])),
            Ok(Command::SessionsEligible(EligibleArgs {
                learner: Harness::ClaudeCode,
                idle: Duration::from_secs(3600),
                max: 10,
                claim: Some(Duration::from_secs(50 * 60)),
            }))
        );
    }

    #[test]
    fn parses_agent_enums_before_dispatch() {
        assert_eq!(
            parse_command(&argv(&[
                "agent",
                "run",
                "--cli",
                "codex",
                "--cleanup",
                "on-success",
                "--timeout",
                "10m",
            ])),
            Ok(Command::AgentRun(AgentRunArgs {
                cli: Harness::Codex,
                model: None,
                args: None,
                label: None,
                cleanup: WorkspaceCleanup::OnSuccess,
                timeout: Duration::from_secs(10 * 60),
                cwd: None,
                idempotency_key: None,
            }))
        );
        assert_eq!(
            parse_command(&argv(&[
                "agent",
                "run",
                "--cli",
                "claude",
                "--idempotency-key",
                "door-key",
            ])),
            Ok(Command::AgentRun(AgentRunArgs {
                cli: Harness::ClaudeCode,
                model: None,
                args: None,
                label: None,
                cleanup: WorkspaceCleanup::Never,
                timeout: Duration::from_secs(45 * 60),
                cwd: None,
                idempotency_key: Some("door-key".to_string()),
            }))
        );
    }

    #[test]
    fn parses_both_reconciler_modes() {
        assert_eq!(
            parse_command(&argv(&["credentials", "reconcile", "--once"])),
            Ok(Command::CredentialsReconcile(ReconcileArgs {
                once: true,
                interval: Duration::from_secs(30),
                source: ReconcileArgs::with_defaults().source,
            }))
        );
        assert_eq!(
            parse_command(&argv(&[
                "credentials",
                "reconcile",
                "--interval",
                "45s",
                "--source",
                "/run/creds",
            ])),
            Ok(Command::CredentialsReconcile(ReconcileArgs {
                once: false,
                interval: Duration::from_secs(45),
                source: PathBuf::from("/run/creds"),
            }))
        );
    }

    /// The guest image probes with `--help` to decide whether this plant build
    /// can reconcile at all, so the probe must exit 0 and not parse as a run.
    #[test]
    fn help_probe_is_recognised_rather_than_rejected() {
        assert_eq!(
            parse_command(&argv(&["credentials", "reconcile", "--help"])),
            Ok(Command::CredentialsHelp)
        );
    }

    #[test]
    fn rejects_every_unmatched_or_invalid_nonempty_form() {
        for args in [
            vec!["--help"],
            vec!["credentials"],
            vec!["credentials", "refresh"],
            vec!["credentials", "reconcile", "--interval"],
            vec!["credentials", "reconcile", "--interval", "soon"],
            vec!["credentials", "reconcile", "--once", "--interval", "30s"],
            vec!["credentials", "reconcile", "--nope"],
            vec!["server"],
            vec!["server", "start"],
            vec!["sessions"],
            vec!["sessions", "stuck", "--age", "bad"],
            vec!["sessions", "eligible", "--learner", "other"],
            vec!["agent", "run", "--cli", "other"],
            vec!["agent", "run", "--cli", "claude", "--cleanup", "sometimes"],
            vec!["jobs", "worker", "name", "path", "0", "1", "10"],
            vec!["--self-test", "extra"],
        ] {
            assert!(parse_command(&argv(&args)).is_err(), "{args:?}");
        }
    }

    #[test]
    fn parses_internal_scheduled_worker_arguments() {
        assert_eq!(
            parse_command(&argv(&[
                "jobs",
                "worker",
                "reconcile",
                "/tmp/reconcile.1h.sh",
                "3600",
                "2",
                "10800",
            ])),
            Ok(Command::JobsWorker(ScheduledWorkerArgs {
                name: "reconcile".to_string(),
                path: PathBuf::from("/tmp/reconcile.1h.sh"),
                every: Duration::from_secs(3600),
                capacity: 2,
                timeout: Duration::from_secs(10800),
            }))
        );
    }
}
