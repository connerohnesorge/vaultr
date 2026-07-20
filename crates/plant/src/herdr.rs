use crate::capture::{cached_session_ids, iso_now, session_dir};
use crate::process::RunResult;
use crate::sweep::run30;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
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
    ready_pane_identity(panes, pane).is_some()
}

#[derive(Debug, PartialEq)]
struct PaneIdentity {
    terminal_id: String,
    agent_session: Option<String>,
}

fn ready_pane_identity(panes: &[Pane], pane: &str) -> Option<PaneIdentity> {
    panes
        .iter()
        .find(|candidate| {
            candidate.pane_id == pane
                && matches!(candidate.agent.as_deref(), Some("claude" | "codex"))
                && matches!(candidate.agent_status.as_str(), "idle" | "done")
        })
        .map(|candidate| PaneIdentity {
            terminal_id: candidate.terminal_id.clone(),
            agent_session: candidate
                .agent_session
                .as_ref()
                .map(|session| session.value.clone()),
        })
}

fn same_pane_identity(expected: &PaneIdentity, panes: &[Pane], pane: &str) -> bool {
    panes.iter().any(|candidate| {
        candidate.pane_id == pane
            && candidate.terminal_id == expected.terminal_id
            && match &expected.agent_session {
                None => true,
                Some(session) => {
                    candidate
                        .agent_session
                        .as_ref()
                        .map(|current| &current.value)
                        == Some(session)
                }
            }
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

struct AgentStartSubscription {
    pane_id: String,
    workspace_id: String,
    reader: BufReader<UnixStream>,
}

async fn subscribe_agent_start_at(
    path: &Path,
    pane_id: &str,
    workspace_id: &str,
) -> Result<AgentStartSubscription, String> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|error| format!("connect: {error}"))?;
        let mut reader = BufReader::new(stream);
        let mut request = serde_json::to_vec(&serde_json::json!({
            "id": "plant-agent-start",
            "method": "events.subscribe",
            "params": {
                "subscriptions": [{
                    "type": "pane.agent_status_changed",
                    "pane_id": pane_id,
                }],
            },
        }))
        .map_err(|error| format!("encode: {error}"))?;
        request.push(b'\n');
        reader
            .get_mut()
            .write_all(&request)
            .await
            .map_err(|error| format!("subscribe: {error}"))?;
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|error| format!("subscribe acknowledgment: {error}"))?;
        let reply: Value =
            serde_json::from_str(&line).map_err(|error| format!("subscribe reply: {error}"))?;
        if reply.pointer("/result/type").and_then(Value::as_str) != Some("subscription_started") {
            return Err("Herdr did not acknowledge the agent-start subscription".to_string());
        }
        Ok(AgentStartSubscription {
            pane_id: pane_id.to_string(),
            workspace_id: workspace_id.to_string(),
            reader,
        })
    })
    .await
    .map_err(|_| "agent-start subscription timed out".to_string())?
}

async fn subscribe_agent_start(
    pane_id: &str,
    workspace_id: &str,
) -> Result<AgentStartSubscription, String> {
    subscribe_agent_start_at(&socket_path(), pane_id, workspace_id).await
}

impl AgentStartSubscription {
    async fn wait(self, timeout: Duration) -> Result<(), String> {
        tokio::time::timeout(timeout, async {
            let mut reader = self.reader;
            let mut working = false;
            loop {
                let mut line = String::new();
                let bytes = reader
                    .read_line(&mut line)
                    .await
                    .map_err(|error| format!("agent-start event: {error}"))?;
                if bytes == 0 {
                    return Err(if working {
                        "agent-start subscription closed before done".to_string()
                    } else {
                        "agent-start subscription closed before working".to_string()
                    });
                }
                let event: Value = serde_json::from_str(&line)
                    .map_err(|error| format!("agent-start event: {error}"))?;
                if event.get("event").and_then(Value::as_str) != Some("pane.agent_status_changed")
                    || event.pointer("/data/pane_id").and_then(Value::as_str)
                        != Some(self.pane_id.as_str())
                    || event.pointer("/data/workspace_id").and_then(Value::as_str)
                        != Some(self.workspace_id.as_str())
                    || !matches!(
                        event.pointer("/data/agent").and_then(Value::as_str),
                        Some("claude" | "codex")
                    )
                {
                    continue;
                }
                match event.pointer("/data/agent_status").and_then(Value::as_str) {
                    Some("working") => working = true,
                    Some("done") if working => return Ok(()),
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| "submitted prompt never completed a working-to-done transition".to_string())?
    }
}

fn require_command(result: &RunResult, action: &str) -> Result<(), String> {
    if result.ok {
        Ok(())
    } else {
        Err(format!("{action} failed ({})", result.failure_detail()))
    }
}

async fn workspaces() -> Option<Vec<Workspace>> {
    let result = run30(&["herdr", "workspace", "list"]).await;
    if !result.ok {
        eprintln!(
            "[herdr] workspace list failed ({})",
            result.failure_detail()
        );
        return None;
    }
    serde_json::from_str::<WorkspaceListReply>(&result.out)
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
    let closed = run30(&["herdr", "workspace", "close", id]).await;
    if !closed.ok {
        eprintln!(
            "[herdr] workspace close {id} failed ({})",
            closed.failure_detail()
        );
    }
    if let Some(previous) = before {
        if previous != id && focused_workspace().await.as_deref() != Some(previous.as_str()) {
            let focused = run30(&["herdr", "workspace", "focus", &previous]).await;
            if !focused.ok {
                eprintln!(
                    "[herdr] workspace focus {previous} failed ({})",
                    focused.failure_detail()
                );
            }
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
    let parsed = created
        .ok
        .then(|| serde_json::from_str::<WorkspaceCreateReply>(&created.out).ok())
        .flatten();
    let workspace_id = parsed
        .as_ref()
        .map(|reply| reply.result.workspace.workspace_id.clone());
    let pane_id = parsed
        .as_ref()
        .map(|reply| reply.result.root_pane.pane_id.clone());

    let outcome = async {
        let (Some(pane), Some(workspace)) = (&pane_id, &workspace_id) else {
            eprintln!(
                "[herdr:{}] workspace create failed ({}): {}",
                agent_run.label,
                created.failure_detail(),
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
        let Some(identity) = pane_list()
            .await
            .and_then(|panes| ready_pane_identity(&panes, pane))
        else {
            return AgentRunOutcome::Failed(
                "CLI pane was not a ready native Claude/Codex agent after settle".to_string(),
            );
        };
        // Herdr 0.7.4 pane.run atomically sends the text plus Enter. Arm and
        // acknowledge one unfiltered status stream first so even a fast
        // working→done turn is buffered on this same pane/workspace cursor.
        let lifecycle = match subscribe_agent_start(pane, workspace).await {
            Ok(subscription) => subscription,
            Err(detail) => {
                return AgentRunOutcome::Failed(format!(
                    "could not observe submitted turn ({detail})"
                ));
            }
        };
        let submitted = run30(&["herdr", "pane", "run", pane, &agent_run.prompt]).await;
        if let Err(detail) = require_command(&submitted, "prompt submission") {
            return AgentRunOutcome::Failed(detail);
        }
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
        if let Err(detail) = require_command(&read, "prompt verification") {
            return AgentRunOutcome::Failed(detail);
        }
        let needle: String = agent_run.prompt.chars().take(30).collect();
        // Claude collapses large pastes to this placeholder; it proves delivery.
        if !read.out.contains(&needle) && !read.out.contains("[Pasted text") {
            return AgentRunOutcome::Failed("submitted prompt was not visible in pane".to_string());
        }
        if let Err(detail) = lifecycle
            .wait(agent_run.timeout + Duration::from_secs(60))
            .await
        {
            return AgentRunOutcome::Failed(format!(
                "submitted prompt did not complete ({detail})"
            ));
        }
        if !pane_list()
            .await
            .is_some_and(|panes| same_pane_identity(&identity, &panes, pane))
        {
            return AgentRunOutcome::Failed(
                "pane terminal/session identity changed during submitted turn".to_string(),
            );
        }
        let tail = run30(&[
            "herdr", "pane", "read", pane, "--source", "recent", "--lines", "15",
        ])
        .await;
        let tail_text = if tail.ok {
            tail.out.trim().to_string()
        } else {
            format!("<tail read failed: {}>", tail.failure_detail())
        };
        println!(
            "[herdr:{}] agent done; tail:\n{}",
            agent_run.label, tail_text
        );
        AgentRunOutcome::Succeeded("agent done".to_string())
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
    use crate::process::RunEnd;

    async fn subscription_server(
        path: PathBuf,
        statuses: Vec<&'static str>,
    ) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            let request: Value = serde_json::from_str(&request).unwrap();
            assert_eq!(
                request.pointer("/params/subscriptions/0"),
                Some(&serde_json::json!({
                    "type": "pane.agent_status_changed",
                    "pane_id": "w1:p1",
                }))
            );
            reader
                .get_mut()
                .write_all(b"{\"id\":\"plant-agent-start\",\"result\":{\"type\":\"subscription_started\"}}\n")
                .await
                .unwrap();
            for status in statuses {
                let mut event = serde_json::to_vec(&serde_json::json!({
                    "event": "pane.agent_status_changed",
                    "data": {
                        "pane_id": "w1:p1",
                        "workspace_id": "w1",
                        "agent_status": status,
                        "agent": "codex",
                    },
                }))
                .unwrap();
                event.push(b'\n');
                reader.get_mut().write_all(&event).await.unwrap();
            }
        })
    }

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

        let mut original = pane(Some("codex"), "done");
        original.agent_session = Some(AgentSession {
            value: "session-1".to_string(),
        });
        let identity = ready_pane_identity(&[original.clone()], "w1:p1").unwrap();
        assert!(same_pane_identity(&identity, &[original.clone()], "w1:p1"));
        original.terminal_id = "replacement".to_string();
        assert!(!same_pane_identity(&identity, &[original], "w1:p1"));
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
    fn failed_atomic_pane_run_is_a_terminal_submission_error() {
        let failed = RunResult {
            ok: false,
            out: String::new(),
            stderr: "pane rejected keys".to_string(),
            end: RunEnd::Exited(Some(1)),
        };
        assert_eq!(
            require_command(&failed, "prompt submission").unwrap_err(),
            "prompt submission failed (exit 1: pane rejected keys)"
        );
    }

    #[tokio::test]
    async fn preexisting_done_without_post_submit_working_is_not_success() {
        let path = PathBuf::from("/tmp").join(format!("ph-{}.sock", uuid::Uuid::new_v4()));
        let server = subscription_server(path.clone(), vec!["done"]).await;
        let subscription = subscribe_agent_start_at(&path, "w1:p1", "w1")
            .await
            .unwrap();
        let error = subscription
            .wait(Duration::from_millis(100))
            .await
            .unwrap_err();
        assert_eq!(
            error, "agent-start subscription closed before working",
            "a pre-submit done observation must not complete this run"
        );
        server.await.unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn acknowledged_working_event_survives_a_fast_turn() {
        let path = PathBuf::from("/tmp").join(format!("ph-{}.sock", uuid::Uuid::new_v4()));
        let server = subscription_server(path.clone(), vec!["working", "done"]).await;
        let subscription = subscribe_agent_start_at(&path, "w1:p1", "w1")
            .await
            .unwrap();
        subscription.wait(Duration::from_millis(100)).await.unwrap();
        server.await.unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a live Herdr session and signed-in Codex CLI"]
    async fn live_agent_lifecycle_preserves_focus_and_cleans_up() {
        let label = format!("plant-lifecycle-smoke-{}", std::process::id());
        let monitoring_label = label.clone();
        let monitor = tokio::spawn(async move {
            let mut seen = false;
            for _ in 0..300 {
                if let Some(workspaces) = workspaces().await {
                    let matching: Vec<_> = workspaces
                        .iter()
                        .filter(|workspace| workspace.label.as_deref() == Some(&monitoring_label))
                        .collect();
                    if matching.iter().any(|workspace| workspace.focused) {
                        return true;
                    }
                    if matching.is_empty() && seen {
                        return false;
                    }
                    seen |= !matching.is_empty();
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            false
        });
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
        assert!(!monitor.await.unwrap(), "smoke workspace stole focus");
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
