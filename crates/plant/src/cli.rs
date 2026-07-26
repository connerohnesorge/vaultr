use crate::domain::Harness;
use crate::herdr::WorkspaceCleanup;
use crate::jobs;
use std::collections::HashSet;
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
    AgentRun(AgentRunArgs),
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
        [group, action, rest @ ..] if group == "sessions" && action == "eligible" => {
            parse_eligible(rest).map(Command::SessionsEligible)
        }
        [group, action, rest @ ..] if group == "agent" && action == "run" => {
            parse_agent(rest).map(Command::AgentRun)
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
    fn rejects_every_unmatched_or_invalid_nonempty_form() {
        for args in [
            vec!["--help"],
            vec!["server"],
            vec!["server", "start"],
            vec!["sessions"],
            vec!["sessions", "stuck", "--age", "bad"],
            vec!["sessions", "eligible", "--learner", "other"],
            vec!["agent", "run", "--cli", "other"],
            vec!["agent", "run", "--cli", "claude", "--cleanup", "sometimes"],
            vec!["--self-test", "extra"],
        ] {
            assert!(parse_command(&argv(&args)).is_err(), "{args:?}");
        }
    }
}
