use crate::capture::{append_herdr_snapshot, cached_session_ids, current_herdr_snapshot, iso_now};
use crate::domain::AgentCli;
use crate::process::{run30, RunResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
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
    /// prime-agent's run-scoped `--session-dir`. Herdr reports no agent_session for a
    /// prime pane, so the id has to come off disk instead: prime writes exactly one
    /// session file into this directory, and that file's first record carries the same
    /// id prime puts in its `session_id` request header — which is the id the wireproxy
    /// files the capture under. The directory is per-run so the file is unambiguous;
    /// prime's own default session dir accumulates every session ever run.
    /// Note the FILENAME is a different uuid from the record id — read the record.
    pub prime_session_dir: Option<PathBuf>,
    /// Forwarded to `herdr workspace create --env KEY=VALUE`. `plant agent run`
    /// runs as a short-lived client process; herdr's own env does not see
    /// vars the caller exported (e.g. `VAULT_PROJECT_DIGEST=0 plant agent run
    /// ...`), so anything the spawned pane's shell must observe has to be
    /// passed explicitly here.
    pub env: Vec<(String, String)>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct AgentRunTarget {
    pub(crate) workspace_id: String,
    pub(crate) pane_id: String,
}

impl AgentRunTarget {
    fn new(workspace_id: &str, pane_id: &str) -> Self {
        Self {
            workspace_id: workspace_id.to_string(),
            pane_id: pane_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum AgentRunPaneIdentity {
    TerminalOnly {
        terminal_id: String,
    },
    SessionBound {
        terminal_id: String,
        session_id: String,
    },
}

impl AgentRunPaneIdentity {
    fn from_pane_identity(identity: &PaneIdentity) -> Self {
        match &identity.agent_session {
            Some(session_id) => Self::SessionBound {
                terminal_id: identity.terminal_id.clone(),
                session_id: session_id.clone(),
            },
            None => Self::TerminalOnly {
                terminal_id: identity.terminal_id.clone(),
            },
        }
    }

    fn terminal_id(&self) -> &str {
        match self {
            Self::TerminalOnly { terminal_id } | Self::SessionBound { terminal_id, .. } => {
                terminal_id
            }
        }
    }

    fn session_id(&self) -> Option<&str> {
        match self {
            Self::TerminalOnly { .. } => None,
            Self::SessionBound { session_id, .. } => Some(session_id),
        }
    }

    fn to_pane_identity(&self) -> PaneIdentity {
        PaneIdentity {
            terminal_id: self.terminal_id().to_string(),
            agent_session: self.session_id().map(str::to_string),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct AgentRunSessionIdentity {
    terminal_id: String,
    session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub(crate) enum AgentRunCheckpoint {
    WorkspaceCreated {
        target: AgentRunTarget,
    },
    Launched {
        target: AgentRunTarget,
    },
    Ready {
        target: AgentRunTarget,
        pane: AgentRunPaneIdentity,
    },
    Submitting {
        target: AgentRunTarget,
        pane: AgentRunPaneIdentity,
    },
    Working {
        target: AgentRunTarget,
        pane: AgentRunPaneIdentity,
    },
    TerminalObserved {
        target: AgentRunTarget,
        pane: AgentRunPaneIdentity,
    },
    Captured {
        target: AgentRunTarget,
        pane: AgentRunSessionIdentity,
    },
}

#[derive(Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
enum AgentRunCheckpointWire {
    WorkspaceCreated {
        target: AgentRunTarget,
    },
    Launched {
        target: AgentRunTarget,
    },
    Ready {
        target: AgentRunTarget,
        pane: AgentRunPaneIdentity,
    },
    Submitting {
        target: AgentRunTarget,
        pane: AgentRunPaneIdentity,
    },
    Working {
        target: AgentRunTarget,
        pane: AgentRunPaneIdentity,
    },
    TerminalObserved {
        target: AgentRunTarget,
        pane: AgentRunPaneIdentity,
    },
    Captured {
        target: AgentRunTarget,
        pane: AgentRunSessionIdentity,
    },
}

impl From<AgentRunCheckpointWire> for AgentRunCheckpoint {
    fn from(checkpoint: AgentRunCheckpointWire) -> Self {
        match checkpoint {
            AgentRunCheckpointWire::WorkspaceCreated { target } => {
                Self::WorkspaceCreated { target }
            }
            AgentRunCheckpointWire::Launched { target } => Self::Launched { target },
            AgentRunCheckpointWire::Ready { target, pane } => Self::Ready { target, pane },
            AgentRunCheckpointWire::Submitting { target, pane } => {
                Self::Submitting { target, pane }
            }
            AgentRunCheckpointWire::Working { target, pane } => Self::Working { target, pane },
            AgentRunCheckpointWire::TerminalObserved { target, pane } => {
                Self::TerminalObserved { target, pane }
            }
            AgentRunCheckpointWire::Captured { target, pane } => Self::Captured { target, pane },
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyAgentRunStage {
    WorkspaceCreated,
    Launched,
    Ready,
    Submitting,
    Working,
    Terminal,
}

#[derive(Deserialize)]
struct LegacyAgentRunIdentity {
    workspace_id: String,
    pane_id: String,
    #[serde(default)]
    terminal_id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    stage: LegacyAgentRunStage,
}

impl AgentRunCheckpoint {
    fn from_legacy(identity: LegacyAgentRunIdentity) -> Result<Self, String> {
        let target = AgentRunTarget::new(&identity.workspace_id, &identity.pane_id);
        match identity.stage {
            LegacyAgentRunStage::WorkspaceCreated => Ok(Self::WorkspaceCreated { target }),
            LegacyAgentRunStage::Launched => Ok(Self::Launched { target }),
            LegacyAgentRunStage::Ready => Ok(Self::Ready {
                target,
                pane: legacy_pane_identity(identity.terminal_id, identity.session_id)?,
            }),
            LegacyAgentRunStage::Submitting => Ok(Self::Submitting {
                target,
                pane: legacy_pane_identity(identity.terminal_id, identity.session_id)?,
            }),
            LegacyAgentRunStage::Working => Ok(Self::Working {
                target,
                pane: legacy_pane_identity(identity.terminal_id, identity.session_id)?,
            }),
            LegacyAgentRunStage::Terminal => Ok(Self::TerminalObserved {
                target,
                pane: legacy_pane_identity(identity.terminal_id, identity.session_id)?,
            }),
        }
    }

    fn workspace_created(workspace_id: &str, pane_id: &str) -> Self {
        Self::WorkspaceCreated {
            target: AgentRunTarget::new(workspace_id, pane_id),
        }
    }

    fn launched(workspace_id: &str, pane_id: &str) -> Self {
        Self::Launched {
            target: AgentRunTarget::new(workspace_id, pane_id),
        }
    }

    fn ready(workspace_id: &str, pane_id: &str, identity: &PaneIdentity) -> Self {
        Self::Ready {
            target: AgentRunTarget::new(workspace_id, pane_id),
            pane: AgentRunPaneIdentity::from_pane_identity(identity),
        }
    }

    fn submitting(workspace_id: &str, pane_id: &str, identity: &PaneIdentity) -> Self {
        Self::Submitting {
            target: AgentRunTarget::new(workspace_id, pane_id),
            pane: AgentRunPaneIdentity::from_pane_identity(identity),
        }
    }

    fn working(workspace_id: &str, pane_id: &str, identity: &PaneIdentity) -> Self {
        Self::Working {
            target: AgentRunTarget::new(workspace_id, pane_id),
            pane: AgentRunPaneIdentity::from_pane_identity(identity),
        }
    }

    fn terminal_observed(workspace_id: &str, pane_id: &str, identity: &PaneIdentity) -> Self {
        Self::TerminalObserved {
            target: AgentRunTarget::new(workspace_id, pane_id),
            pane: AgentRunPaneIdentity::from_pane_identity(identity),
        }
    }

    fn captured(
        workspace_id: &str,
        pane_id: &str,
        identity: &PaneIdentity,
        session_id: &str,
    ) -> Self {
        Self::Captured {
            target: AgentRunTarget::new(workspace_id, pane_id),
            pane: AgentRunSessionIdentity {
                terminal_id: identity.terminal_id.clone(),
                session_id: session_id.to_string(),
            },
        }
    }

    pub(crate) fn target(&self) -> &AgentRunTarget {
        match self {
            Self::WorkspaceCreated { target }
            | Self::Launched { target }
            | Self::Ready { target, .. }
            | Self::Submitting { target, .. }
            | Self::Working { target, .. }
            | Self::TerminalObserved { target, .. }
            | Self::Captured { target, .. } => target,
        }
    }

    fn pane_identity(&self) -> Option<AgentRunPaneIdentity> {
        match self {
            Self::WorkspaceCreated { .. } | Self::Launched { .. } => None,
            Self::Ready { pane, .. }
            | Self::Submitting { pane, .. }
            | Self::Working { pane, .. }
            | Self::TerminalObserved { pane, .. } => Some(pane.clone()),
            Self::Captured { pane, .. } => Some(AgentRunPaneIdentity::SessionBound {
                terminal_id: pane.terminal_id.clone(),
                session_id: pane.session_id.clone(),
            }),
        }
    }

    pub(crate) fn session_id(&self) -> Option<&str> {
        match self {
            Self::WorkspaceCreated { .. } | Self::Launched { .. } => None,
            Self::Ready { pane, .. }
            | Self::Submitting { pane, .. }
            | Self::Working { pane, .. }
            | Self::TerminalObserved { pane, .. } => pane.session_id(),
            Self::Captured { pane, .. } => Some(&pane.session_id),
        }
    }

    pub(crate) fn rank(&self) -> u8 {
        match self {
            Self::WorkspaceCreated { .. } => 0,
            Self::Launched { .. } => 1,
            Self::Ready { .. } => 2,
            Self::Submitting { .. } => 3,
            Self::Working { .. } => 4,
            Self::TerminalObserved { .. } => 5,
            Self::Captured { .. } => 6,
        }
    }

    fn has_submitted_work(&self) -> bool {
        matches!(
            self,
            Self::Working { .. } | Self::TerminalObserved { .. } | Self::Captured { .. }
        )
    }

    pub(crate) fn can_follow(&self, next: &Self) -> bool {
        if self.target() != next.target() {
            return false;
        }
        if self.rank() == next.rank() {
            return same_pane_checkpoint(self.pane_identity(), next.pane_identity());
        }
        match (self.pane_identity(), next.pane_identity()) {
            (None, _) => true,
            (Some(current), Some(next)) => pane_can_follow(&current, &next),
            (Some(_), None) => self.rank() > next.rank(),
        }
    }
}

impl<'de> Deserialize<'de> for AgentRunCheckpoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        if value.get("target").is_some() {
            serde_json::from_value::<AgentRunCheckpointWire>(value)
                .map(Into::into)
                .map_err(|error| serde::de::Error::custom(error.to_string()))
        } else {
            let legacy = serde_json::from_value::<LegacyAgentRunIdentity>(value)
                .map_err(|error| serde::de::Error::custom(error.to_string()))?;
            Self::from_legacy(legacy).map_err(serde::de::Error::custom)
        }
    }
}

fn legacy_pane_identity(
    terminal_id: Option<String>,
    session_id: Option<String>,
) -> Result<AgentRunPaneIdentity, String> {
    let Some(terminal_id) = terminal_id else {
        return Err("pending Agent Run has no terminal identity".to_string());
    };
    Ok(match session_id {
        Some(session_id) => AgentRunPaneIdentity::SessionBound {
            terminal_id,
            session_id,
        },
        None => AgentRunPaneIdentity::TerminalOnly { terminal_id },
    })
}

fn same_pane_checkpoint(
    current: Option<AgentRunPaneIdentity>,
    next: Option<AgentRunPaneIdentity>,
) -> bool {
    match (current, next) {
        (None, None) => true,
        (Some(current), Some(next)) => pane_can_follow(&current, &next),
        _ => false,
    }
}

fn pane_can_follow(current: &AgentRunPaneIdentity, next: &AgentRunPaneIdentity) -> bool {
    match (current, next) {
        (
            AgentRunPaneIdentity::TerminalOnly {
                terminal_id: current,
            },
            AgentRunPaneIdentity::TerminalOnly { terminal_id: next },
        ) => current == next,
        (
            AgentRunPaneIdentity::TerminalOnly {
                terminal_id: current,
            },
            AgentRunPaneIdentity::SessionBound {
                terminal_id: next, ..
            },
        ) => current == next,
        (
            AgentRunPaneIdentity::SessionBound {
                terminal_id: current_terminal,
                session_id: current_session,
            },
            AgentRunPaneIdentity::SessionBound {
                terminal_id: next_terminal,
                session_id: next_session,
            },
        ) => current_terminal == next_terminal && current_session == next_session,
        (AgentRunPaneIdentity::SessionBound { .. }, AgentRunPaneIdentity::TerminalOnly { .. }) => {
            false
        }
    }
}

pub(crate) trait AgentRunProgress: Send + Sync {
    fn update(&self, checkpoint: AgentRunCheckpoint) -> Result<(), String>;
}

fn update_progress(
    progress: Option<&dyn AgentRunProgress>,
    checkpoint: AgentRunCheckpoint,
) -> Result<(), String> {
    progress.map_or(Ok(()), |progress| progress.update(checkpoint))
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AgentRunObservation {
    Terminal,
    Absent,
    Retain(String),
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
#[cfg(test)]
fn pane_accepts_prompt(panes: &[Pane], pane: &str) -> bool {
    ready_pane_identity(panes, pane).is_some()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PaneIdentity {
    terminal_id: String,
    agent_session: Option<String>,
}

fn ready_pane_identity(panes: &[Pane], pane: &str) -> Option<PaneIdentity> {
    panes
        .iter()
        .find(|candidate| {
            candidate.pane_id == pane
                && AgentCli::is_known_herdr_agent(candidate.agent.as_deref())
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

fn pane_is_working(panes: &[Pane], pane: &str) -> bool {
    panes
        .iter()
        .any(|candidate| candidate.pane_id == pane && candidate.agent_status == "working")
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Did the typed prompt actually reach the composer?
///
/// A pane reports `idle` as soon as the harness process is up, which on a box
/// where that harness has never launched is BEFORE its TUI can accept
/// keystrokes. `pane run` then types into nothing, the text is lost, the agent
/// goes straight to `done`, and the run fails as "agent reached a terminal
/// state without a capture session id" — a message that says nothing about
/// typing. Reproduced on both harnesses on a freshly provisioned box; the
/// identical command succeeds on the next (warm) run every time.
///
/// Only the head of the prompt is compared, against a whitespace-collapsed
/// pane read, because the composer wraps long text. Callers must read with
/// `--source recent-unwrapped` so a wrap does not split a word mid-needle.
fn composer_shows_prompt(pane_text: &str, prompt: &str) -> bool {
    let needle: String = collapse_whitespace(prompt).chars().take(24).collect();
    if needle.trim().is_empty() {
        return true;
    }
    collapse_whitespace(pane_text).contains(&needle)
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

fn settled_ready_identity(
    expected: &PaneIdentity,
    panes: &[Pane],
    pane: &str,
) -> Result<Option<PaneIdentity>, String> {
    if !same_pane_identity(expected, panes, pane) {
        return Err("pane terminal/session identity changed during readiness".to_string());
    }
    Ok(ready_pane_identity(panes, pane))
}

fn snapshot_completed(
    identity: &PaneIdentity,
    panes: &[Pane],
    pane: &str,
    working: &mut bool,
) -> Result<bool, String> {
    if !same_pane_identity(identity, panes, pane) {
        return Err("pane terminal/session identity changed during submitted turn".to_string());
    }
    *working |= pane_is_working(panes, pane);
    Ok(*working && ready_pane_identity(panes, pane).is_some())
}

async fn wait_for_prompt_ready(pane: &str, timeout: Duration) -> Result<PaneIdentity, String> {
    tokio::time::timeout(timeout, async {
        loop {
            let Some(panes) = pane_list().await else {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            };
            let Some(identity) = ready_pane_identity(&panes, pane) else {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            };
            tokio::time::sleep(Duration::from_secs(3)).await;
            let Some(panes) = pane_list().await else {
                continue;
            };
            if let Some(identity) = settled_ready_identity(&identity, &panes, pane)? {
                return Ok(identity);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .map_err(|_| "native Claude/Codex pane did not reach idle or done".to_string())?
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
    async fn wait(
        self,
        identity: &PaneIdentity,
        timeout: Duration,
        initial_working: bool,
        progress: Option<&dyn AgentRunProgress>,
    ) -> Result<(), String> {
        tokio::time::timeout(timeout, async {
            let mut reader = self.reader;
            let mut working = initial_working;
            let mut snapshots = tokio::time::interval(Duration::from_millis(250));
            snapshots.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = snapshots.tick() => {
                        let Some(panes) = pane_list().await else { continue };
                        let was_working = working;
                        if snapshot_completed(identity, &panes, &self.pane_id, &mut working)? {
                            return Ok(());
                        }
                        if !was_working && working {
                            update_progress(
                                progress,
                                AgentRunCheckpoint::working(
                                    &self.workspace_id,
                                    &self.pane_id,
                                    identity,
                                ),
                            )?;
                        }
                    }
                    line = async {
                        let mut line = String::new();
                        let bytes = reader.read_line(&mut line).await?;
                        Ok::<_, std::io::Error>((bytes, line))
                    } => {
                        let (bytes, line) = line.map_err(|error| format!("agent-start event: {error}"))?;
                        if bytes == 0 && working {
                            return Err("agent-start subscription closed before completion".to_string());
                        }
                        if bytes == 0 {
                            return Err("agent-start subscription closed before working".to_string());
                        }
                        let event: Value = serde_json::from_str(&line)
                            .map_err(|error| format!("agent-start event: {error}"))?;
                        if event.get("event").and_then(Value::as_str) != Some("pane.agent_status_changed")
                            || event.pointer("/data/pane_id").and_then(Value::as_str) != Some(self.pane_id.as_str())
                            || event.pointer("/data/workspace_id").and_then(Value::as_str) != Some(self.workspace_id.as_str())
                            || !AgentCli::is_known_herdr_agent(event.pointer("/data/agent").and_then(Value::as_str)) {
                            continue;
                        }
                        match event.pointer("/data/agent_status").and_then(Value::as_str) {
                            Some("working") => {
                                if !working {
                                    update_progress(
                                        progress,
                                        AgentRunCheckpoint::working(
                                            &self.workspace_id,
                                            &self.pane_id,
                                            identity,
                                        ),
                                    )?;
                                }
                                working = true;
                            }
                            Some("done" | "idle") if working => return Ok(()),
                            _ => {}
                        }
                    }
                }
            }
        })
        .await
        .map_err(|_| {
            "submitted prompt never reached a terminal state after working".to_string()
        })?
    }
}

/// Read the pane unwrapped and report whether the prompt is sitting in it.
/// A failed read returns `true` so an unrelated herdr hiccup can never cause a
/// duplicate prompt — the long lifecycle wait still reports the real outcome.
async fn prompt_reached_composer(pane: &str, prompt: &str) -> bool {
    let read = run30(&[
        "herdr",
        "pane",
        "read",
        pane,
        "--source",
        "recent-unwrapped",
        "--lines",
        "40",
    ])
    .await;
    if !read.ok {
        return true;
    }
    composer_shows_prompt(&read.out, prompt)
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
    let Some(previous) = before else {
        return;
    };
    if previous == id || focused_workspace().await.as_deref() == Some(previous.as_str()) {
        return;
    }
    let focused = run30(&["herdr", "workspace", "focus", &previous]).await;
    if !focused.ok {
        eprintln!(
            "[herdr] workspace focus {previous} failed ({})",
            focused.failure_detail()
        );
    }
}

/// The capture session id prime-agent used, read from its run-scoped session dir.
///
/// prime writes the session file as the run winds down, so this polls briefly rather
/// than reading once. Returns None rather than guessing if the directory holds no
/// session file or more than one — a wrong id here would be registered as plant's own
/// self-capture and silently drop a real session from learning.
fn prime_session_id(dir: &Path) -> Option<String> {
    let mut sessions = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"));
    let session = sessions.next()?;
    if sessions.next().is_some() {
        return None;
    }
    let first_record =
        std::io::BufRead::lines(std::io::BufReader::new(std::fs::File::open(&session).ok()?))
            .next()?
            .ok()?;
    serde_json::from_str::<Value>(&first_record)
        .ok()?
        .get("id")
        .and_then(Value::as_str)
        .map(String::from)
}

async fn register_discovered_session_id(
    label: &str,
    pane: Option<&str>,
    discover: bool,
    prime_session_dir: Option<&Path>,
) -> Option<String> {
    if let Some(dir) = prime_session_dir {
        for _ in 0..5 {
            let Some(sid) = prime_session_id(dir) else {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            };
            crate::sweep::register_job_sid(&sid);
            println!("[herdr:{label}] registered job self-capture {sid}");
            return Some(sid);
        }
        eprintln!(
            "[herdr:{label}] no prime session file under {}; self-capture may reach learn",
            dir.display()
        );
        return None;
    }
    if !discover {
        return None;
    }
    let pane = pane?;
    for _ in 0..3 {
        let Some(sid) = pane_list()
            .await
            .and_then(|panes| pick_session_id(&panes, pane))
        else {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        };
        crate::sweep::register_job_sid(&sid);
        println!("[herdr:{label}] registered job self-capture {sid}");
        return Some(sid);
    }
    eprintln!("[herdr:{label}] no agent_session id for pane {pane}; self-capture may reach learn");
    None
}

fn recovery_pane_identity(checkpoint: &AgentRunCheckpoint) -> Result<PaneIdentity, String> {
    let Some(pane) = checkpoint.pane_identity() else {
        return Err("pending Agent Run has no terminal identity".to_string());
    };
    if pane.session_id().is_none() {
        return Err("pending Agent Run has no captured session identity".to_string());
    }
    Ok(pane.to_pane_identity())
}

#[derive(Debug, PartialEq, Eq)]
enum RecordedPane {
    Working,
    Terminal,
    Absent,
}

fn classify_recorded_pane(
    panes: &[Pane],
    checkpoint: &AgentRunCheckpoint,
    expected: &PaneIdentity,
) -> Result<RecordedPane, String> {
    let exact = panes.iter().find(|pane| {
        pane.workspace_id == checkpoint.target().workspace_id
            && pane.pane_id == checkpoint.target().pane_id
    });
    let Some(pane) = exact else {
        if panes.iter().any(|pane| {
            pane.pane_id == checkpoint.target().pane_id
                || pane.workspace_id == checkpoint.target().workspace_id
        }) {
            return Err(
                "recorded workspace or pane identity conflicts with live Herdr state".to_string(),
            );
        }
        return Ok(RecordedPane::Absent);
    };
    if !AgentCli::is_known_herdr_agent(pane.agent.as_deref()) {
        return Err("recorded pane is no longer a native agent pane".to_string());
    }
    if pane.terminal_id != expected.terminal_id
        || pane.agent_session.as_ref().map(|session| &session.value)
            != expected.agent_session.as_ref()
    {
        return Err("recorded pane terminal or session identity conflicts".to_string());
    }
    if pane.agent_status == "working" {
        return Ok(RecordedPane::Working);
    }
    if matches!(pane.agent_status.as_str(), "idle" | "done") && checkpoint.has_submitted_work() {
        return Ok(RecordedPane::Terminal);
    }
    if matches!(pane.agent_status.as_str(), "idle" | "done") {
        return Err("recorded Agent Run has no observed submitted work".to_string());
    }
    Err(format!(
        "recorded pane has unrecognized lifecycle state {}",
        pane.agent_status
    ))
}

/// Observe a pending keyed Agent Run without creating a workspace.
///
/// The exact workspace, pane, terminal, and captured session identity is the
/// recovery boundary. Missing, conflicting, or unavailable evidence remains
/// pending. This function reports only Herdr execution evidence. The Agent Run
/// coordinator decides whether the matching Vault capture makes that evidence
/// conclusive.
pub(crate) async fn observe_agent_run(
    checkpoint: &AgentRunCheckpoint,
    progress: Option<&dyn AgentRunProgress>,
) -> AgentRunObservation {
    let expected = match recovery_pane_identity(checkpoint) {
        Ok(expected) => expected,
        Err(detail) => return AgentRunObservation::Retain(detail),
    };
    let Some(panes) = pane_list().await else {
        return AgentRunObservation::Retain("Herdr pane identity is unavailable".to_string());
    };
    match classify_recorded_pane(&panes, checkpoint, &expected) {
        Ok(RecordedPane::Working) => {
            if let Err(detail) = update_progress(
                progress,
                AgentRunCheckpoint::working(
                    &checkpoint.target().workspace_id,
                    &checkpoint.target().pane_id,
                    &expected,
                ),
            ) {
                return AgentRunObservation::Retain(detail);
            }
            let subscription = match subscribe_agent_start(
                &checkpoint.target().pane_id,
                &checkpoint.target().workspace_id,
            )
            .await
            {
                Ok(subscription) => subscription,
                Err(detail) => return AgentRunObservation::Retain(detail),
            };
            if let Err(detail) = subscription
                .wait(&expected, Duration::from_secs(3 * 3600), true, progress)
                .await
            {
                return AgentRunObservation::Retain(format!(
                    "recorded pane observation did not complete ({detail})"
                ));
            }
            let Some(current) = pane_list().await else {
                return AgentRunObservation::Retain(
                    "Herdr pane identity is unavailable after completion".to_string(),
                );
            };
            let Some(current) = current.iter().find(|pane| {
                pane.workspace_id == checkpoint.target().workspace_id
                    && pane.pane_id == checkpoint.target().pane_id
            }) else {
                return AgentRunObservation::Retain(
                    "recorded pane disappeared before terminal identity verification".to_string(),
                );
            };
            if current.terminal_id != expected.terminal_id
                || current.agent_session.as_ref().map(|session| &session.value)
                    != expected.agent_session.as_ref()
            {
                return AgentRunObservation::Retain(
                    "recorded pane terminal or session identity changed".to_string(),
                );
            }
            if let Err(detail) = update_progress(
                progress,
                AgentRunCheckpoint::terminal_observed(
                    &checkpoint.target().workspace_id,
                    &checkpoint.target().pane_id,
                    &expected,
                ),
            ) {
                return AgentRunObservation::Retain(detail);
            }
            return AgentRunObservation::Terminal;
        }
        Ok(RecordedPane::Terminal) => {
            if let Err(detail) = update_progress(
                progress,
                AgentRunCheckpoint::terminal_observed(
                    &checkpoint.target().workspace_id,
                    &checkpoint.target().pane_id,
                    &expected,
                ),
            ) {
                return AgentRunObservation::Retain(detail);
            }
            return AgentRunObservation::Terminal;
        }
        Ok(RecordedPane::Absent) => {}
        Err(detail) => return AgentRunObservation::Retain(detail),
    }
    let Some(workspaces) = workspaces().await else {
        return AgentRunObservation::Retain("Herdr workspace identity is unavailable".to_string());
    };
    if workspaces
        .iter()
        .any(|workspace| workspace.workspace_id == checkpoint.target().workspace_id)
    {
        return AgentRunObservation::Retain(
            "recorded workspace remains but its pane identity is unavailable".to_string(),
        );
    }
    if !checkpoint.has_submitted_work() {
        return AgentRunObservation::Retain(
            "recorded Agent Run has no observed submitted work".to_string(),
        );
    }
    AgentRunObservation::Absent
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
    run_agent_with_progress(agent_run, None).await
}

pub(crate) async fn run_agent_with_progress(
    agent_run: AgentRun,
    progress: Option<&dyn AgentRunProgress>,
) -> AgentRunOutcome {
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
    let env_flags: Vec<String> = agent_run
        .env
        .iter()
        .flat_map(|(k, v)| ["--env".to_string(), format!("{k}={v}")])
        .collect();
    let mut create_cmd: Vec<&str> = vec![
        "herdr",
        "workspace",
        "create",
        "--cwd",
        &agent_run.cwd,
        "--label",
        &agent_run.label,
        "--no-focus",
    ];
    create_cmd.extend(env_flags.iter().map(String::as_str));
    let created = run30(&create_cmd).await;
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
        if let Err(detail) = update_progress(
            progress,
            AgentRunCheckpoint::workspace_created(workspace, pane),
        ) {
            return AgentRunOutcome::Failed(format!(
                "could not persist workspace identity ({detail})"
            ));
        }
        let launched = run30(&["herdr", "pane", "run", pane, &agent_run.launch]).await;
        if !launched.ok {
            return AgentRunOutcome::Failed(format!(
                "pane run failed ({})",
                launched.failure_detail()
            ));
        }
        if let Err(detail) =
            update_progress(progress, AgentRunCheckpoint::launched(workspace, pane))
        {
            return AgentRunOutcome::Failed(format!(
                "could not persist launched identity ({detail})"
            ));
        }
        let mut identity = match wait_for_prompt_ready(pane, Duration::from_secs(60)).await {
            Ok(identity) => identity,
            Err(detail) => {
                eprintln!(
                    "[herdr:{}] agent did not become ready in pane {pane} ({detail})",
                    agent_run.label
                );
                return AgentRunOutcome::Failed(format!("agent never became ready ({detail})"));
            }
        };
        if identity.agent_session.is_none() {
            identity.agent_session = agent_run.preset_session_id.clone();
        }
        if let Err(detail) = update_progress(
            progress,
            AgentRunCheckpoint::ready(workspace, pane, &identity),
        ) {
            return AgentRunOutcome::Failed(format!("could not persist ready identity ({detail})"));
        }
        // Arm the status observer BEFORE typing so even a fast working→done turn
        // is buffered on this pane/workspace cursor. Herdr `pane run` only TYPES
        // the prompt: a running Claude TUI needs an explicit Enter to submit,
        // while Codex auto-runs a recognized slash-command prompt (e.g. `/goal`)
        // the instant it is typed. Send one Enter only if the agent has not
        // already started, then let the buffered observer arbitrate delivery.
        // The literal prompt text is NOT a reliable signal: Codex rewrites a
        // slash-command into its own status line, so it never echoes verbatim —
        // a working→done transition is the real proof the prompt landed.
        let lifecycle = match subscribe_agent_start(pane, workspace).await {
            Ok(subscription) => subscription,
            Err(detail) => {
                return AgentRunOutcome::Failed(format!(
                    "could not observe submitted turn ({detail})"
                ));
            }
        };
        if let Err(detail) = update_progress(
            progress,
            AgentRunCheckpoint::submitting(workspace, pane, &identity),
        ) {
            return AgentRunOutcome::Failed(format!(
                "could not persist submission identity ({detail})"
            ));
        }
        let typed = run30(&["herdr", "pane", "run", pane, &agent_run.prompt]).await;
        if let Err(detail) = require_command(&typed, "prompt typing") {
            return AgentRunOutcome::Failed(detail);
        }
        // An immediate Enter is swallowed by the slash-command palette; let the
        // composer settle before deciding whether a submit keystroke is needed.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let mut already_working = pane_list()
            .await
            .is_some_and(|panes| pane_is_working(&panes, pane));
        // A cold TUI can swallow the whole prompt (see `composer_shows_prompt`).
        // Retype only when the agent has NOT started and the text is demonstrably
        // absent: a blind retype would append a second copy whenever the first
        // one did land. An unreadable pane counts as present for the same reason.
        if !already_working && !prompt_reached_composer(pane, &agent_run.prompt).await {
            eprintln!(
                "[herdr:{}] prompt did not reach the composer in {pane}, retyping",
                agent_run.label
            );
            let retyped = run30(&["herdr", "pane", "run", pane, &agent_run.prompt]).await;
            if let Err(detail) = require_command(&retyped, "prompt retyping") {
                return AgentRunOutcome::Failed(detail);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            already_working = pane_list()
                .await
                .is_some_and(|panes| pane_is_working(&panes, pane));
        }
        if !already_working {
            let submitted = run30(&["herdr", "pane", "send-keys", pane, "Enter"]).await;
            if let Err(detail) = require_command(&submitted, "prompt submission") {
                return AgentRunOutcome::Failed(detail);
            }
        }
        if let Err(detail) = lifecycle
            .wait(
                &identity,
                agent_run.timeout + Duration::from_secs(60),
                false,
                progress,
            )
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
        if let Err(detail) = update_progress(
            progress,
            AgentRunCheckpoint::terminal_observed(workspace, pane, &identity),
        ) {
            return AgentRunOutcome::Failed(format!(
                "could not persist terminal identity ({detail})"
            ));
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
            "[herdr:{}] agent terminal state observed; tail:\n{}",
            agent_run.label, tail_text
        );
        AgentRunOutcome::Succeeded("agent done".to_string())
    }
    .await;

    // Register this pane's self-capture before cleanup can close it, so no learn pass
    // ever dispatches on plant's own housekeeping run. Claude preset its sid pre-launch;
    // codex ids are only knowable now, from what herdr reports for the pane.
    let discovered_session_id = register_discovered_session_id(
        &agent_run.label,
        pane_id.as_deref(),
        agent_run.discover_session_id,
        agent_run.prime_session_dir.as_deref(),
    )
    .await;
    let session_id = agent_run
        .preset_session_id
        .clone()
        .or(discovered_session_id);
    let mut outcome = outcome;
    if matches!(outcome, AgentRunOutcome::Succeeded(_)) {
        if let (Some(progress), Some(workspace), Some(pane), Some(session_id)) = (
            progress,
            workspace_id.as_deref(),
            pane_id.as_deref(),
            session_id.as_deref(),
        ) {
            let pane_identity = pane_list()
                .await
                .and_then(|panes| ready_pane_identity(&panes, pane));
            match pane_identity {
                Some(pane_identity) => {
                    let captured =
                        AgentRunCheckpoint::captured(workspace, pane, &pane_identity, session_id);
                    if let Err(detail) = progress.update(captured) {
                        outcome = AgentRunOutcome::Failed(format!(
                            "could not persist captured session identity ({detail})"
                        ));
                    }
                }
                None => {
                    outcome = AgentRunOutcome::Failed(
                        "could not persist captured session identity: terminal identity unavailable"
                            .to_string(),
                    );
                }
            }
        }
    }
    let outcome = match (outcome, session_id) {
        (AgentRunOutcome::Succeeded(_), Some(session_id)) => {
            match crate::agent_run::wait_for_completed_envelope(&crate::vault_root(), &session_id)
                .await
            {
                Ok(()) => AgentRunOutcome::Succeeded("agent done".to_string()),
                Err(detail) => AgentRunOutcome::Failed(detail),
            }
        }
        (AgentRunOutcome::Succeeded(_), None) => AgentRunOutcome::Failed(
            "agent reached a terminal state without a capture session id".to_string(),
        ),
        (outcome, _) => outcome,
    };
    match (
        should_cleanup(agent_run.cleanup, &outcome),
        workspace_id.as_deref(),
    ) {
        (true, Some(id)) => close_workspace(id).await,
        (false, Some(_)) => println!(
            "[herdr:{}] pane kept open (cleanup: {:?})",
            agent_run.label, agent_run.cleanup
        ),
        _ => {}
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
                    let entry = state.entry(sid.clone()).or_insert_with(|| {
                        (
                            now.checked_sub(wait).unwrap_or(now),
                            current_herdr_snapshot(&vault, sid),
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
            if SNAPSHOTS
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|state| state.get(&sid))
                .is_none_or(|entry| entry.1 == sans_ts)
            {
                continue;
            }
            let Ok(line) = serde_json::to_string(&TimestampedSnapshot {
                ts: iso_now(),
                pane: &snapshot.pane,
                siblings: &snapshot.siblings,
            }) else {
                continue;
            };
            if append_herdr_snapshot(&vault, &sid, &sans_ts, &line)
                .await
                .is_ok_and(|appended| appended)
            {
                if let Some(entry) = SNAPSHOTS
                    .lock()
                    .unwrap()
                    .get_or_insert_with(HashMap::new)
                    .get_mut(&sid)
                {
                    entry.1 = sans_ts;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::RunEnd;

    fn prime_session_fixture(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("plant-prime-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for (file, body) in files {
            std::fs::write(dir.join(file), body).unwrap();
        }
        dir
    }

    /// The id plant needs is the one in the session file's FIRST RECORD, not the one in
    /// the filename — prime mints those separately and only the record id matches the
    /// `session_id` header the wireproxy files the capture under. Reading the filename
    /// would produce a plausible uuid that matches no capture, and plant would then call
    /// every prime run failed for want of a completed envelope.
    #[test]
    fn prime_session_id_reads_the_record_not_the_filename() {
        let dir = prime_session_fixture(
            "record",
            &[(
                "019fe795-14e4-77ed-8917-b9b58fce1a9c.jsonl",
                "{\"id\":\"019fe795-1bf1-746f-a76d-e273c683a524\",\"kind\":\"session\"}\n\
                 {\"id\":\"later-record-ignored\"}\n",
            )],
        );
        assert_eq!(
            prime_session_id(&dir).as_deref(),
            Some("019fe795-1bf1-746f-a76d-e273c683a524")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Ambiguity must not be resolved by guessing: registering the wrong id marks a real
    /// interactive session as plant's own self-capture, and learn then skips it forever.
    #[test]
    fn prime_session_id_refuses_an_ambiguous_or_empty_directory() {
        let empty = prime_session_fixture("empty", &[]);
        assert_eq!(prime_session_id(&empty), None);
        std::fs::remove_dir_all(empty).unwrap();

        let two = prime_session_fixture(
            "two",
            &[
                ("a.jsonl", "{\"id\":\"aaa\"}\n"),
                ("b.jsonl", "{\"id\":\"bbb\"}\n"),
            ],
        );
        assert_eq!(prime_session_id(&two), None);
        std::fs::remove_dir_all(two).unwrap();

        // present but not yet flushed — caller retries rather than binding to nothing
        let empty_file = prime_session_fixture("partial", &[("a.jsonl", "")]);
        assert_eq!(prime_session_id(&empty_file), None);
        std::fs::remove_dir_all(empty_file).unwrap();

        assert_eq!(
            prime_session_id(Path::new("/nonexistent/plant/prime")),
            None
        );
    }

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

    fn codex_pane(status: &str) -> Pane {
        Pane {
            workspace_id: "w1".to_string(),
            tab_id: "w1:t1".to_string(),
            pane_id: "w1:p1".to_string(),
            terminal_id: "t1".to_string(),
            cwd: "/tmp".to_string(),
            focused: false,
            agent_status: status.to_string(),
            agent: Some("codex".to_string()),
            agent_session: None,
        }
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

    fn recovery_checkpoint(stage: &str) -> AgentRunCheckpoint {
        serde_json::from_value(serde_json::json!({
            "stage": stage,
            "target": {"workspace_id": "w1", "pane_id": "w1:p1"},
            "pane": {
                "kind": "session_bound",
                "terminal_id": "t1",
                "session_id": "session-1"
            }
        }))
        .unwrap()
    }

    fn recovered_pane(status: &str, session: &str) -> Pane {
        let mut pane = codex_pane(status);
        pane.agent_session = Some(AgentSession {
            value: session.to_string(),
        });
        pane
    }

    #[test]
    fn recovery_requires_the_exact_live_execution_identity() {
        let working = recovery_checkpoint("working");
        let expected = recovery_pane_identity(&working).unwrap();
        assert_eq!(
            classify_recorded_pane(
                &[recovered_pane("working", "session-1")],
                &working,
                &expected
            ),
            Ok(RecordedPane::Working)
        );
        assert_eq!(
            classify_recorded_pane(&[recovered_pane("done", "session-1")], &working, &expected),
            Ok(RecordedPane::Terminal)
        );
        assert!(classify_recorded_pane(
            &[recovered_pane("working", "other-session")],
            &working,
            &expected
        )
        .unwrap_err()
        .contains("conflicts"));
        assert_eq!(
            classify_recorded_pane(&[], &working, &expected),
            Ok(RecordedPane::Absent)
        );

        let ready = recovery_checkpoint("ready");
        let expected = recovery_pane_identity(&ready).unwrap();
        assert!(
            classify_recorded_pane(&[recovered_pane("idle", "session-1")], &ready, &expected)
                .unwrap_err()
                .contains("submitted work")
        );
    }

    #[test]
    fn tagged_checkpoints_encode_phase_specific_identity() {
        let terminal_only = serde_json::from_value::<AgentRunCheckpoint>(serde_json::json!({
            "stage": "working",
            "target": {"workspace_id": "w1", "pane_id": "w1:p1"},
            "pane": {"kind": "terminal_only", "terminal_id": "t1"}
        }))
        .unwrap();
        assert_eq!(terminal_only.session_id(), None);
        assert!(recovery_pane_identity(&terminal_only).is_err());

        let captured = serde_json::from_value::<AgentRunCheckpoint>(serde_json::json!({
            "stage": "captured",
            "target": {"workspace_id": "w1", "pane_id": "w1:p1"},
            "pane": {"terminal_id": "t1", "session_id": "session-1"}
        }))
        .unwrap();
        assert_eq!(captured.session_id(), Some("session-1"));
        assert!(recovery_pane_identity(&captured).is_ok());
        assert!(terminal_only.can_follow(&captured));

        let replaced_session = serde_json::from_value::<AgentRunCheckpoint>(serde_json::json!({
            "stage": "captured",
            "target": {"workspace_id": "w1", "pane_id": "w1:p1"},
            "pane": {"terminal_id": "t1", "session_id": "other-session"}
        }))
        .unwrap();
        assert!(!captured.can_follow(&replaced_session));

        let legacy = serde_json::from_str::<AgentRunCheckpoint>(
            r#"{"workspace_id":"w1","pane_id":"w1:p1","terminal_id":"t1","session_id":"session-1","stage":"working"}"#,
        )
        .unwrap();
        assert!(matches!(legacy, AgentRunCheckpoint::Working { .. }));
        assert_eq!(legacy.session_id(), Some("session-1"));
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
        for agent in ["claude", "codex"] {
            for (status, expected) in [
                ("idle", true),
                ("done", true),
                ("working", false),
                ("blocked", false),
                ("unknown", false),
            ] {
                assert_eq!(
                    pane_accepts_prompt(&[pane(Some(agent), status)], "w1:p1"),
                    expected,
                    "{agent}/{status}"
                );
            }
        }
        for agent in [Some("unknown"), None] {
            for status in ["idle", "done"] {
                assert!(
                    !pane_accepts_prompt(&[pane(agent, status)], "w1:p1"),
                    "{agent:?}/{status}"
                );
            }
        }
        assert!(!pane_accepts_prompt(
            &[pane(Some("codex"), "done")],
            "other"
        ));

        let shell = pane(None, "idle");
        let mut ready_sibling = pane(Some("codex"), "done");
        ready_sibling.pane_id = "w1:p2".to_string();
        assert!(!pane_accepts_prompt(&[shell, ready_sibling], "w1:p1"));

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
    fn empty_composer_is_detected_so_a_cold_tui_gets_the_prompt_again() {
        let prompt = "$Vault Learn with `--learner codex` and these session directories \
                      as input: /home/dev/.dotfiles/vault/sessions/2026/07/16/8f9aa81d";

        // What a cold pane actually showed: the harness up, the composer empty
        // apart from its placeholder, zero tokens spent.
        let cold = "\n\n› Improve documentation in @filename\n\n  gpt-5.6-sol xhigh · ~/dotfiles";
        assert!(
            !composer_shows_prompt(cold, prompt),
            "an empty composer must not read as delivered"
        );

        // Delivered, and wrapped across lines the way the composer renders it.
        let wrapped = "› $Vault Learn with `--learner\n  codex` and these session\n  directories as input: /home/…";
        assert!(
            composer_shows_prompt(wrapped, prompt),
            "a wrapped prompt is present and must NOT be retyped — a second copy \
             would hand the agent the same instruction twice"
        );

        // A prompt that is entirely whitespace has no needle to look for; never
        // retype on it.
        assert!(composer_shows_prompt(cold, "   \n  "));
    }

    #[test]
    fn pane_is_working_matches_only_the_named_working_pane() {
        // Codex auto-submits a slash-command prompt; a `working` pane is the
        // signal to skip the explicit Enter that a typed Claude prompt needs.
        let at = |pane_id: &str, status: &str| Pane {
            workspace_id: "w1".to_string(),
            tab_id: "w1:t1".to_string(),
            pane_id: pane_id.to_string(),
            terminal_id: "t1".to_string(),
            cwd: "/tmp".to_string(),
            focused: false,
            agent_status: status.to_string(),
            agent: Some("codex".to_string()),
            agent_session: None,
        };
        assert!(pane_is_working(&[at("w1:p1", "working")], "w1:p1"));
        assert!(!pane_is_working(&[at("w1:p1", "idle")], "w1:p1"));
        assert!(!pane_is_working(&[at("w1:p1", "done")], "w1:p1"));
        assert!(!pane_is_working(&[at("w1:p2", "working")], "w1:p1"));
    }

    #[test]
    fn readiness_reconciles_a_transient_non_ready_snapshot() {
        let identity = ready_pane_identity(&[codex_pane("idle")], "w1:p1").unwrap();
        assert_eq!(
            settled_ready_identity(&identity, &[codex_pane("working")], "w1:p1").unwrap(),
            None
        );
        assert!(
            settled_ready_identity(&identity, &[codex_pane("idle")], "w1:p1")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn snapshot_terminal_completes_after_working_without_terminal_event() {
        let identity = ready_pane_identity(&[codex_pane("idle")], "w1:p1").unwrap();
        let mut working = false;
        assert!(
            !snapshot_completed(&identity, &[codex_pane("working")], "w1:p1", &mut working)
                .unwrap()
        );
        assert!(
            snapshot_completed(&identity, &[codex_pane("idle")], "w1:p1", &mut working).unwrap()
        );
    }

    #[test]
    fn snapshot_terminal_without_working_is_not_success() {
        let identity = ready_pane_identity(&[codex_pane("idle")], "w1:p1").unwrap();
        let mut working = false;
        assert!(
            !snapshot_completed(&identity, &[codex_pane("done")], "w1:p1", &mut working).unwrap()
        );
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
    fn failed_enter_send_keys_is_a_terminal_submission_error() {
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
        let identity = PaneIdentity {
            terminal_id: "t1".to_string(),
            agent_session: None,
        };
        let path = PathBuf::from("/tmp").join(format!("ph-{}.sock", uuid::Uuid::new_v4()));
        let server = subscription_server(path.clone(), vec!["done"]).await;
        let subscription = subscribe_agent_start_at(&path, "w1:p1", "w1")
            .await
            .unwrap();
        let error = subscription
            .wait(&identity, Duration::from_millis(100), false, None)
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
        let identity = PaneIdentity {
            terminal_id: "t1".to_string(),
            agent_session: None,
        };
        let path = PathBuf::from("/tmp").join(format!("ph-{}.sock", uuid::Uuid::new_v4()));
        let server = subscription_server(path.clone(), vec!["working", "done"]).await;
        let subscription = subscribe_agent_start_at(&path, "w1:p1", "w1")
            .await
            .unwrap();
        subscription
            .wait(&identity, Duration::from_millis(100), false, None)
            .await
            .unwrap();
        server.await.unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn resumed_working_pane_accepts_the_next_terminal_event() {
        let identity = PaneIdentity {
            terminal_id: "t1".to_string(),
            agent_session: Some("session-1".to_string()),
        };
        let path = PathBuf::from("/tmp").join(format!("ph-{}.sock", uuid::Uuid::new_v4()));
        let server = subscription_server(path.clone(), vec!["done"]).await;
        let subscription = subscribe_agent_start_at(&path, "w1:p1", "w1")
            .await
            .unwrap();
        subscription
            .wait(&identity, Duration::from_millis(100), true, None)
            .await
            .unwrap();
        server.await.unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn working_then_idle_completes_for_codex() {
        let identity = PaneIdentity {
            terminal_id: "t1".to_string(),
            agent_session: None,
        };
        // Codex settles a finished non-goal turn to `idle`, not `done`; the
        // observer must treat that as completion or the whole job hangs.
        let path = PathBuf::from("/tmp").join(format!("ph-{}.sock", uuid::Uuid::new_v4()));
        let server = subscription_server(path.clone(), vec!["working", "idle"]).await;
        let subscription = subscribe_agent_start_at(&path, "w1:p1", "w1")
            .await
            .unwrap();
        subscription
            .wait(&identity, Duration::from_millis(100), false, None)
            .await
            .unwrap();
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
                let Some(workspaces) = workspaces().await else {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                };
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
            discover_session_id: true,
            prime_session_dir: None,
            env: Vec::new(),
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
