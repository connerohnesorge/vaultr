use crate::capture::{cached_session_ids, iso_now, session_dir};
use crate::sweep::{run, run30};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Clone, Deserialize)]
pub(crate) struct Pane {
    workspace_id: String,
    tab_id: String,
    pane_id: String,
    terminal_id: String,
    cwd: String,
    focused: bool,
    agent_status: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    agent_session: Option<AgentSession>,
}

#[derive(Clone, Deserialize)]
struct AgentSession {
    value: String,
}

#[derive(Deserialize)]
struct PaneListReply {
    result: PaneListResult,
}

#[derive(Deserialize)]
struct PaneListResult {
    panes: Vec<Pane>,
}

#[derive(Deserialize)]
struct WorkspaceListReply {
    result: WorkspaceListResult,
}

#[derive(Deserialize)]
struct WorkspaceListResult {
    workspaces: Vec<Workspace>,
}

#[derive(Deserialize)]
struct Workspace {
    workspace_id: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    agent_status: Option<String>,
}

#[derive(Deserialize)]
struct WorkspaceCreateReply {
    result: WorkspaceCreateResult,
}

#[derive(Deserialize)]
struct WorkspaceCreateResult {
    workspace: CreatedWorkspace,
    root_pane: CreatedPane,
}

#[derive(Deserialize)]
struct CreatedWorkspace {
    workspace_id: String,
}

#[derive(Deserialize)]
struct CreatedPane {
    pane_id: String,
}

#[derive(Serialize)]
struct BoundPane {
    workspace_id: String,
    tab_id: String,
    pane_id: String,
    terminal_id: String,
    cwd: String,
    focused: bool,
    agent_status: String,
}

#[derive(Serialize)]
struct Sibling {
    pane_id: String,
    cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_session_id: Option<String>,
    agent_status: String,
}

#[derive(Serialize)]
struct Snapshot {
    pane: BoundPane,
    siblings: Vec<Sibling>,
}

#[derive(Serialize)]
struct TimestampedSnapshot<'a> {
    ts: String,
    pane: &'a BoundPane,
    siblings: &'a [Sibling],
}

static SNAPSHOTS: Mutex<Option<HashMap<String, (Instant, String)>>> = Mutex::new(None);

pub struct AgentRun {
    pub label: String,
    pub cwd: String,
    pub launch: String,
    pub prompt: String,
    pub timeout: Duration,
    pub cleanup: WorkspaceCleanup,
    /// Claude session ids are minted with the launch command, but registration
    /// waits until this run actually passes any idempotency lookup.
    pub preset_session_id: Option<String>,
    /// Claude sids are preset + registered before launch; codex conversation ids are
    /// server-assigned, so for codex we read the id herdr reports for this pane once the
    /// run finishes and register it, keeping learn from dispatching on the self-capture.
    pub discover_session_id: bool,
}

/// The agent_session id herdr reports for `pane_id`, if any. herdr surfaces the same
/// value the wireproxy files the capture under (see `maybe_snapshot` correlation), so
/// registering it as a job sid matches the capture in learn eligibility.
fn pick_session_id(panes: &[Pane], pane_id: &str) -> Option<String> {
    panes
        .iter()
        .find(|p| p.pane_id == pane_id)
        .and_then(|p| p.agent_session.as_ref())
        .map(|s| s.value.clone())
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WorkspaceCleanup {
    Never,
    Always,
    OnSuccess,
}

#[derive(Debug, PartialEq)]
pub enum AgentRunOutcome {
    Unavailable,
    Succeeded(String),
    Failed(String),
}

#[derive(Debug, PartialEq)]
pub enum DurableAgentOutcome {
    Succeeded(String),
    Failed(String),
}

#[derive(Debug, PartialEq)]
pub enum IdempotentAgentRun {
    Durable(DurableAgentOutcome),
    Retryable(String),
    Indeterminate(String),
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum DurableAgentRun {
    InProgress { key: String },
    Succeeded { key: String, detail: String },
    Failed { key: String, detail: String },
}

enum IdempotencyClaim {
    Execute(PathBuf),
    Prior(DurableAgentOutcome),
    Pending,
}

fn idempotency_path(dir: &Path, key: &str) -> io::Result<PathBuf> {
    if key.is_empty() || key.len() > 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "idempotency key must contain 1..=1024 bytes",
        ));
    }
    Ok(dir.join(format!("{:x}.json", Sha256::digest(key.as_bytes()))))
}

fn claim_agent_run(dir: &Path, key: &str) -> io::Result<IdempotencyClaim> {
    std::fs::create_dir_all(dir)?;
    let path = idempotency_path(dir, key)?;
    let record = DurableAgentRun::InProgress {
        key: key.to_string(),
    };
    let mut bytes =
        serde_json::to_vec(&record).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            let result = file
                .write_all(&bytes)
                .and_then(|_| file.sync_all())
                .and_then(|_| crate::jobs::sync_dir(dir));
            if let Err(error) = result {
                let _ = std::fs::remove_file(&path);
                return Err(error);
            }
            Ok(IdempotencyClaim::Execute(path))
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let record: DurableAgentRun = serde_json::from_str(&std::fs::read_to_string(&path)?)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let (recorded_key, claim) = match record {
                DurableAgentRun::InProgress { key } => (key, IdempotencyClaim::Pending),
                DurableAgentRun::Succeeded { key, detail } => (
                    key,
                    IdempotencyClaim::Prior(DurableAgentOutcome::Succeeded(detail)),
                ),
                DurableAgentRun::Failed { key, detail } => (
                    key,
                    IdempotencyClaim::Prior(DurableAgentOutcome::Failed(detail)),
                ),
            };
            if recorded_key != key {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "idempotency digest collision",
                ));
            }
            Ok(claim)
        }
        Err(error) => Err(error),
    }
}

fn persist_agent_outcome(path: &Path, key: &str, outcome: &DurableAgentOutcome) -> io::Result<()> {
    let record = match outcome {
        DurableAgentOutcome::Succeeded(detail) => DurableAgentRun::Succeeded {
            key: key.to_string(),
            detail: detail.clone(),
        },
        DurableAgentOutcome::Failed(detail) => DurableAgentRun::Failed {
            key: key.to_string(),
            detail: detail.clone(),
        },
    };
    let mut bytes =
        serde_json::to_vec(&record).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    crate::jobs::atomic_write(path, &bytes)
}

/// Run an agent at most once for a caller-supplied stable key. Conclusive
/// outcomes are replayed without touching Herdr; uncertain state fails closed.
pub(crate) async fn run_agent_idempotent(agent_run: AgentRun, key: &str) -> IdempotentAgentRun {
    let dir = crate::jobs::state_dir().join("agent-runs");
    let path = match claim_agent_run(&dir, key) {
        Ok(IdempotencyClaim::Execute(path)) => path,
        Ok(IdempotencyClaim::Prior(outcome)) => return IdempotentAgentRun::Durable(outcome),
        Ok(IdempotencyClaim::Pending) => {
            return IdempotentAgentRun::Indeterminate(
                "idempotent agent run has no conclusive outcome; refusing duplicate launch"
                    .to_string(),
            )
        }
        Err(error) => {
            return IdempotentAgentRun::Indeterminate(format!(
                "idempotency state unavailable: {error}"
            ))
        }
    };

    let outcome = match run_agent(agent_run).await {
        AgentRunOutcome::Unavailable => {
            return match std::fs::remove_file(&path).and_then(|_| crate::jobs::sync_dir(&dir)) {
                Ok(()) => {
                    IdempotentAgentRun::Retryable("herdr unavailable before launch".to_string())
                }
                Err(error) => IdempotentAgentRun::Indeterminate(format!(
                    "could not release unavailable idempotency claim: {error}"
                )),
            };
        }
        AgentRunOutcome::Succeeded(detail) => DurableAgentOutcome::Succeeded(detail),
        AgentRunOutcome::Failed(detail) => DurableAgentOutcome::Failed(detail),
    };
    match persist_agent_outcome(&path, key, &outcome) {
        Ok(()) => IdempotentAgentRun::Durable(outcome),
        Err(error) => IdempotentAgentRun::Indeterminate(format!(
            "could not persist conclusive agent outcome: {error}"
        )),
    }
}

fn socket_path() -> PathBuf {
    std::env::var("HERDR_SOCKET_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".config/herdr/herdr.sock")
        })
}

pub(crate) async fn pane_list() -> Option<Vec<Pane>> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut stream = UnixStream::connect(socket_path()).await.ok()?;
        stream
            .write_all(b"{\"id\":\"plant\",\"method\":\"pane.list\",\"params\":{}}\n")
            .await
            .ok()?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.ok()?;
        serde_json::from_str::<PaneListReply>(&line)
            .ok()
            .map(|r| r.result.panes)
    })
    .await
    .ok()
    .flatten()
}

/// Herdr reports `idle` for a bare shell too. Only native Claude/Codex panes in
/// a prompt-safe state may receive TUI input.
fn pane_accepts_prompt(panes: &[Pane], pane: &str) -> bool {
    panes.iter().any(|candidate| {
        candidate.pane_id == pane
            && matches!(candidate.agent.as_deref(), Some("claude" | "codex"))
            && matches!(candidate.agent_status.as_str(), "idle" | "done")
    })
}

async fn wait_for_prompt_ready(pane: &str, timeout: Duration) -> bool {
    tokio::time::timeout(timeout, async {
        loop {
            if pane_list()
                .await
                .is_some_and(|panes| pane_accepts_prompt(&panes, pane))
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .unwrap_or(false)
}

async fn workspaces() -> Option<Vec<Workspace>> {
    serde_json::from_str::<WorkspaceListReply>(&run30(&["herdr", "workspace", "list"]).await.out)
        .ok()
        .map(|reply| reply.result.workspaces)
}

async fn focused_workspace() -> Option<String> {
    workspaces()
        .await?
        .into_iter()
        .find(|workspace| workspace.focused)
        .map(|workspace| workspace.workspace_id)
}

/// `workspace close` may refocus another workspace even when the closed one was
/// unfocused. Restore the user's prior focus when that happens.
async fn close_workspace(id: &str) {
    let before = focused_workspace().await;
    run30(&["herdr", "workspace", "close", id]).await;
    if let Some(previous) = before {
        if previous != id && focused_workspace().await.as_deref() != Some(previous.as_str()) {
            run30(&["herdr", "workspace", "focus", &previous]).await;
        }
    }
}

/// Reclaim inactive workspaces left by interrupted or intentionally retained runs.
async fn close_by_label(label: &str) -> u32 {
    let Some(workspaces) = workspaces().await else {
        return 0;
    };
    let mut closed = 0;
    for workspace in workspaces {
        if workspace.label.as_deref() != Some(label)
            || workspace.agent_status.as_deref() == Some("working")
        {
            continue;
        }
        close_workspace(&workspace.workspace_id).await;
        closed += 1;
    }
    closed
}

fn should_cleanup(cleanup: WorkspaceCleanup, outcome: &AgentRunOutcome) -> bool {
    match cleanup {
        WorkspaceCleanup::Never => false,
        WorkspaceCleanup::Always => !matches!(outcome, AgentRunOutcome::Unavailable),
        WorkspaceCleanup::OnSuccess => matches!(outcome, AgentRunOutcome::Succeeded(_)),
    }
}

/// Create an unfocused Herdr workspace, run and verify an agent, deliver one
/// prompt, wait for completion, and apply the requested best-effort cleanup.
pub(crate) async fn run_agent(agent_run: AgentRun) -> AgentRunOutcome {
    let probe = run30(&["herdr", "workspace", "list"]).await;
    if !probe.ok {
        println!(
            "[herdr:{}] unavailable ({})",
            agent_run.label,
            probe.failure_detail()
        );
        return AgentRunOutcome::Unavailable;
    }
    if let Some(session_id) = &agent_run.preset_session_id {
        crate::sweep::register_job_sid(session_id);
    }
    let stale = close_by_label(&agent_run.label).await;
    if stale > 0 {
        println!(
            "[herdr:{}] reclaimed {stale} stale workspace(s)",
            agent_run.label
        );
    }
    let created = run30(&[
        "herdr",
        "workspace",
        "create",
        "--cwd",
        &agent_run.cwd,
        "--label",
        &agent_run.label,
        "--no-focus",
    ])
    .await;
    let parsed = serde_json::from_str::<WorkspaceCreateReply>(&created.out).ok();
    let workspace_id = parsed
        .as_ref()
        .map(|reply| reply.result.workspace.workspace_id.clone());
    let pane_id = parsed
        .as_ref()
        .map(|reply| reply.result.root_pane.pane_id.clone());

    let outcome = async {
        let (Some(pane), Some(_)) = (&pane_id, &workspace_id) else {
            eprintln!(
                "[herdr:{}] workspace create failed: {}",
                agent_run.label,
                created.out.chars().take(200).collect::<String>()
            );
            let orphans = close_by_label(&agent_run.label).await;
            return AgentRunOutcome::Failed(if orphans > 0 {
                format!("workspace create unparseable, {orphans} orphan(s) closed by label")
            } else {
                "workspace create failed".to_string()
            });
        };
        let launched = run30(&["herdr", "pane", "run", pane, &agent_run.launch]).await;
        if !launched.ok {
            return AgentRunOutcome::Failed(format!(
                "pane run failed ({})",
                launched.failure_detail()
            ));
        }
        if !wait_for_prompt_ready(pane, Duration::from_secs(60)).await {
            let detail = "native Claude/Codex pane did not reach idle or done";
            eprintln!(
                "[herdr:{}] agent did not become ready in pane {pane} ({detail})",
                agent_run.label
            );
            return AgentRunOutcome::Failed(format!("agent never became ready ({detail})"));
        }
        // Agent detection precedes TUI input readiness by ~1-2 seconds.
        tokio::time::sleep(Duration::from_secs(3)).await;
        if !pane_list()
            .await
            .is_some_and(|panes| pane_accepts_prompt(&panes, pane))
        {
            return AgentRunOutcome::Failed(
                "CLI pane was not a ready native Claude/Codex agent after settle".to_string(),
            );
        }
        let needle: String = agent_run.prompt.chars().take(30).collect();
        let mut landed = false;
        let mut read_failure = None;
        for _ in 0..3 {
            run30(&["herdr", "pane", "run", pane, &agent_run.prompt]).await;
            tokio::time::sleep(Duration::from_secs(2)).await;
            let read = run30(&[
                "herdr",
                "pane",
                "read",
                pane,
                "--source",
                "recent-unwrapped",
                "--lines",
                "80",
            ])
            .await;
            if !read.ok {
                read_failure = Some(read.failure_detail());
                break;
            }
            // Claude collapses large pastes to this placeholder; it proves delivery.
            if read.out.contains(&needle) || read.out.contains("[Pasted text") {
                landed = true;
                break;
            }
        }
        if !landed {
            return AgentRunOutcome::Failed(match read_failure {
                Some(detail) => format!("prompt never landed in pane (pane read: {detail})"),
                None => "prompt never landed in pane".to_string(),
            });
        }
        run30(&["herdr", "pane", "send-keys", pane, "Enter"]).await;
        run30(&["herdr", "pane", "send-keys", pane, "Enter"]).await;
        let timeout_ms = agent_run.timeout.as_millis().to_string();
        let done = run(
            &[
                "herdr",
                "wait",
                "agent-status",
                pane,
                "--status",
                "done",
                "--timeout",
                &timeout_ms,
            ],
            agent_run.timeout + Duration::from_secs(60),
        )
        .await;
        let tail = run30(&[
            "herdr", "pane", "read", pane, "--source", "recent", "--lines", "15",
        ])
        .await;
        let detail = (!done.ok).then(|| done.failure_detail());
        println!(
            "[herdr:{}] agent {}; tail:\n{}",
            agent_run.label,
            match &detail {
                None => "done".to_string(),
                Some(d) => format!("FAILED ({d})"),
            },
            tail.out.trim()
        );
        match detail {
            None => AgentRunOutcome::Succeeded("agent done".to_string()),
            Some(d) => AgentRunOutcome::Failed(format!("agent wait failed ({d})")),
        }
    }
    .await;

    // Register this pane's self-capture before cleanup can close it, so no learn pass
    // ever dispatches on plant's own housekeeping run. Claude preset its sid pre-launch;
    // codex ids are only knowable now, from what herdr reports for the pane.
    if agent_run.discover_session_id {
        if let Some(pane) = &pane_id {
            let mut registered = None;
            for _ in 0..3 {
                if let Some(sid) = pane_list().await.and_then(|p| pick_session_id(&p, pane)) {
                    crate::sweep::register_job_sid(&sid);
                    registered = Some(sid);
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            match registered {
                Some(sid) => println!(
                    "[herdr:{}] registered job self-capture {sid}",
                    agent_run.label
                ),
                None => eprintln!(
                    "[herdr:{}] no agent_session id for pane {pane}; self-capture may reach learn",
                    agent_run.label
                ),
            }
        }
    }

    if should_cleanup(agent_run.cleanup, &outcome) {
        if let Some(id) = &workspace_id {
            close_workspace(id).await;
        }
    } else if workspace_id.is_some() {
        println!(
            "[herdr:{}] pane kept open (cleanup: {:?})",
            agent_run.label, agent_run.cleanup
        );
    }
    outcome
}

fn interval() -> Duration {
    Duration::from_secs(
        std::env::var("PLANT_HERDR_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60),
    )
}

fn last_snapshot(path: &Path) -> String {
    use std::io::BufRead;
    let Some(line) = std::fs::File::open(path)
        .ok()
        .and_then(|f| std::io::BufReader::new(f).lines().last())
        .and_then(Result::ok)
    else {
        return String::new();
    };
    let Ok(mut value) = serde_json::from_str::<Value>(&line) else {
        return String::new();
    };
    let Some(object) = value.as_object_mut() else {
        return String::new();
    };
    object.remove("ts");
    serde_json::to_string(&value).unwrap_or_default()
}

fn snapshot(bound: &Pane, panes: &[Pane]) -> Snapshot {
    Snapshot {
        pane: BoundPane {
            workspace_id: bound.workspace_id.clone(),
            tab_id: bound.tab_id.clone(),
            pane_id: bound.pane_id.clone(),
            terminal_id: bound.terminal_id.clone(),
            cwd: bound.cwd.clone(),
            focused: bound.focused,
            agent_status: bound.agent_status.clone(),
        },
        siblings: panes
            .iter()
            .filter(|p| p.workspace_id == bound.workspace_id && p.pane_id != bound.pane_id)
            .map(|p| Sibling {
                pane_id: p.pane_id.clone(),
                cwd: p.cwd.clone(),
                agent: p.agent.clone(),
                agent_session_id: p.agent_session.as_ref().map(|s| s.value.clone()),
                agent_status: p.agent_status.clone(),
            })
            .collect(),
    }
}

pub fn maybe_snapshot(vault: &Path) {
    let vault = vault.to_path_buf();
    tokio::spawn(async move {
        let now = Instant::now();
        let wait = interval();
        let eligible: Vec<String> = {
            let mut guard = SNAPSHOTS.lock().unwrap();
            let state = guard.get_or_insert_with(HashMap::new);
            cached_session_ids(&vault)
                .into_iter()
                .filter(|sid| {
                    let path = session_dir(&vault, sid).ok().map(|d| d.join("herdr.jsonl"));
                    let entry = state.entry(sid.clone()).or_insert_with(|| {
                        (
                            now.checked_sub(wait).unwrap_or(now),
                            path.as_deref().map(last_snapshot).unwrap_or_default(),
                        )
                    });
                    if now.duration_since(entry.0) < wait {
                        return false;
                    }
                    entry.0 = now;
                    true
                })
                .collect()
        };
        if eligible.is_empty() {
            return;
        }
        let Some(panes) = pane_list().await else {
            return;
        };
        for sid in eligible {
            let Some(bound) = panes
                .iter()
                .find(|p| p.agent_session.as_ref().is_some_and(|s| s.value == sid))
            else {
                continue;
            };
            let snapshot = snapshot(bound, &panes);
            let Ok(sans_ts) = serde_json::to_string(&snapshot) else {
                continue;
            };
            let mut guard = SNAPSHOTS.lock().unwrap();
            let state = guard.get_or_insert_with(HashMap::new);
            let Some(entry) = state.get_mut(&sid) else {
                continue;
            };
            if entry.1 == sans_ts {
                continue;
            }
            let Ok(dir) = session_dir(&vault, &sid) else {
                continue;
            };
            let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("herdr.jsonl"))
            else {
                continue;
            };
            let Ok(line) = serde_json::to_string(&TimestampedSnapshot {
                ts: iso_now(),
                pane: &snapshot.pane,
                siblings: &snapshot.siblings,
            }) else {
                continue;
            };
            if writeln!(file, "{}", line).is_ok() {
                entry.1 = sans_ts;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_typed_panes_and_workspaces() {
        let panes: PaneListReply = serde_json::from_str(
            r#"{"result":{"panes":[{"workspace_id":"w1","tab_id":"w1:t1","pane_id":"w1:p1","terminal_id":"t1","cwd":"/tmp","focused":true,"agent_status":"idle","agent":"codex","agent_session":{"value":"session-1"}}]}}"#,
        )
        .unwrap();
        let pane = &panes.result.panes[0];
        assert_eq!(pane.pane_id, "w1:p1");
        assert_eq!(pane.agent.as_deref(), Some("codex"));

        // codex self-capture discovery: the pane's server-assigned session id is picked
        // by pane_id, and an unknown/agentless pane yields nothing to register.
        assert_eq!(
            pick_session_id(&panes.result.panes, "w1:p1").as_deref(),
            Some("session-1")
        );
        assert_eq!(pick_session_id(&panes.result.panes, "nope"), None);

        let workspaces: WorkspaceListReply = serde_json::from_str(
            r#"{"result":{"workspaces":[{"workspace_id":"w1","label":"job-smoke","focused":false,"agent_status":"idle"}]}}"#,
        )
        .unwrap();
        let workspace = &workspaces.result.workspaces[0];
        assert_eq!(workspace.label.as_deref(), Some("job-smoke"));
        assert!(!workspace.focused);

        let created: WorkspaceCreateReply = serde_json::from_str(
            r#"{"result":{"workspace":{"workspace_id":"w2"},"root_pane":{"pane_id":"w2:p1"}}}"#,
        )
        .unwrap();
        assert_eq!(created.result.workspace.workspace_id, "w2");
        assert_eq!(created.result.root_pane.pane_id, "w2:p1");
    }

    #[test]
    fn prompt_readiness_requires_native_idle_or_done_agent() {
        let pane = |agent: Option<&str>, status: &str| Pane {
            workspace_id: "w1".to_string(),
            tab_id: "w1:t1".to_string(),
            pane_id: "w1:p1".to_string(),
            terminal_id: "t1".to_string(),
            cwd: "/tmp".to_string(),
            focused: false,
            agent_status: status.to_string(),
            agent: agent.map(str::to_string),
            agent_session: None,
        };
        for (agent, status, expected) in [
            (Some("claude"), "idle", true),
            (Some("claude"), "done", true),
            (Some("codex"), "idle", true),
            (Some("codex"), "done", true),
            (Some("claude"), "working", false),
            (Some("codex"), "blocked", false),
            (Some("codex"), "unknown", false),
            (Some("unknown"), "idle", false),
            (None, "idle", false),
        ] {
            assert_eq!(
                pane_accepts_prompt(&[pane(agent, status)], "w1:p1"),
                expected,
                "{agent:?}/{status}"
            );
        }
        assert!(!pane_accepts_prompt(
            &[pane(Some("codex"), "done")],
            "other"
        ));
    }

    #[test]
    fn cleanup_follows_policy_and_outcome() {
        let succeeded = AgentRunOutcome::Succeeded("done".into());
        let failed = AgentRunOutcome::Failed("timeout".into());
        assert!(!should_cleanup(WorkspaceCleanup::Never, &succeeded));
        assert!(should_cleanup(WorkspaceCleanup::Always, &failed));
        assert!(should_cleanup(WorkspaceCleanup::OnSuccess, &succeeded));
        assert!(!should_cleanup(WorkspaceCleanup::OnSuccess, &failed));
        assert!(!should_cleanup(
            WorkspaceCleanup::Always,
            &AgentRunOutcome::Unavailable
        ));
    }

    #[test]
    fn idempotency_state_replays_outcomes_and_fails_closed() {
        let dir = std::env::temp_dir().join(format!(
            "plant-agent-runs-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let path = match claim_agent_run(&dir, "door-batch") {
            Ok(IdempotencyClaim::Execute(path)) => path,
            _ => panic!("first claim should execute"),
        };
        assert!(matches!(
            claim_agent_run(&dir, "door-batch").unwrap(),
            IdempotencyClaim::Pending
        ));
        persist_agent_outcome(
            &path,
            "door-batch",
            &DurableAgentOutcome::Succeeded("done once".to_string()),
        )
        .unwrap();
        assert!(matches!(
            claim_agent_run(&dir, "door-batch").unwrap(),
            IdempotencyClaim::Prior(DurableAgentOutcome::Succeeded(ref detail))
                if detail == "done once"
        ));

        let corrupt = match claim_agent_run(&dir, "corrupt") {
            Ok(IdempotencyClaim::Execute(path)) => path,
            _ => panic!("first corrupt claim should execute"),
        };
        std::fs::write(corrupt, "{").unwrap();
        assert!(claim_agent_run(&dir, "corrupt").is_err());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a live Herdr session and signed-in Codex CLI"]
    async fn live_agent_lifecycle_preserves_focus_and_cleans_up() {
        let focused_before = focused_workspace().await.expect("focused Herdr workspace");
        let label = format!("plant-lifecycle-smoke-{}", std::process::id());
        let outcome = run_agent(AgentRun {
            label: label.clone(),
            cwd: std::env::current_dir().unwrap().display().to_string(),
            launch: "command codex --dangerously-bypass-approvals-and-sandbox -c model_reasoning_effort=low".into(),
            prompt: "Reply with exactly HERDR_LIFECYCLE_SMOKE_OK and do not use tools.".into(),
            timeout: Duration::from_secs(120),
            cleanup: WorkspaceCleanup::Always,
            preset_session_id: None,
            discover_session_id: false,
        })
        .await;

        assert!(
            matches!(outcome, AgentRunOutcome::Succeeded(_)),
            "{outcome:?}"
        );
        assert_eq!(
            focused_workspace().await.as_deref(),
            Some(focused_before.as_str())
        );
        assert!(
            workspaces()
                .await
                .unwrap_or_default()
                .iter()
                .all(|workspace| workspace.label.as_deref() != Some(&label)),
            "smoke workspace was not cleaned up"
        );
    }
}
