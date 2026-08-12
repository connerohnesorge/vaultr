use crate::herdr::{
    observe_agent_run, run_agent_with_progress, AgentRun, AgentRunCheckpoint, AgentRunObservation,
    AgentRunOutcome, AgentRunProgress,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum AgentRunReceipt {
    Succeeded { detail: String },
    Failed { detail: String },
    UntrackedSucceeded { detail: String },
    UntrackedFailed { detail: String },
    Retryable { detail: String },
    Indeterminate { detail: String },
}

impl AgentRunReceipt {
    pub(crate) fn untracked(outcome: AgentRunOutcome) -> Self {
        match outcome {
            AgentRunOutcome::Succeeded(detail) => Self::UntrackedSucceeded { detail },
            AgentRunOutcome::Unavailable => Self::Retryable {
                detail: "herdr unavailable".to_string(),
            },
            AgentRunOutcome::Failed(detail) => Self::UntrackedFailed { detail },
        }
    }

    pub(crate) fn exit_code(&self) -> i32 {
        match self {
            Self::Succeeded { .. } | Self::UntrackedSucceeded { .. } => 0,
            Self::Retryable { .. } => 75,
            Self::Failed { .. } | Self::UntrackedFailed { .. } | Self::Indeterminate { .. } => 1,
        }
    }

    fn durable(&self) -> bool {
        matches!(self, Self::Succeeded { .. } | Self::Failed { .. })
    }

    /// Job-ledger outcome name and detail for a conclusive receipt.
    pub(crate) fn ledger_outcome(&self) -> Option<(&'static str, &str)> {
        match self {
            Self::Succeeded { detail } => Some(("success", detail)),
            Self::Failed { detail } => Some(("failed", detail)),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum AgentRunRecovery {
    Succeeded(String),
    Failed(String),
    Retain(String),
}

fn capture_has_completed_envelope(path: &Path) -> Result<bool, String> {
    let mut completed = false;
    vaultr::recon::for_each_envelope(path, |envelope| {
        completed |= envelope
            .pointer("/response/complete")
            .and_then(Value::as_bool)
            == Some(true);
        Ok(())
    })
    .map_err(|error| error.to_string())?;
    Ok(completed)
}

fn session_has_completed_envelope(vault: &Path, session_id: &str) -> Result<bool, String> {
    let session =
        vaultr::vault::resolve_id(vault, session_id).map_err(|error| error.to_string())?;
    let directory =
        vaultr::vault::session_dir(vault, &session).map_err(|error| error.to_string())?;
    let capture = vaultr::vault::capture_file(&directory).map_err(|error| error.to_string())?;
    capture_has_completed_envelope(&capture)
}

pub(crate) async fn wait_for_completed_envelope(
    vault: &Path,
    session_id: &str,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match session_has_completed_envelope(vault, session_id) {
            Ok(true) => return Ok(()),
            Ok(false) if Instant::now() >= deadline => {
                return Err(format!(
                    "session {session_id} produced no completed capture envelope"
                ));
            }
            Err(error) if Instant::now() >= deadline => {
                return Err(format!(
                    "could not inspect capture for session {session_id}: {error}"
                ));
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

async fn recovered_capture_outcome_at(
    vault: &Path,
    checkpoint: &AgentRunCheckpoint,
) -> AgentRunRecovery {
    let Some(session_id) = checkpoint.session_id() else {
        return AgentRunRecovery::Retain(
            "pending Agent Run has no captured session identity".to_string(),
        );
    };
    match wait_for_completed_envelope(vault, session_id).await {
        Ok(()) => AgentRunRecovery::Succeeded(
            "agent completed; recovered from the matching terminal capture".to_string(),
        ),
        Err(detail) if detail.contains("produced no completed capture envelope") => {
            AgentRunRecovery::Failed(format!("recorded Agent Run cannot finish: {detail}"))
        }
        Err(detail) => AgentRunRecovery::Retain(detail),
    }
}

async fn recovered_capture_outcome(checkpoint: &AgentRunCheckpoint) -> AgentRunRecovery {
    recovered_capture_outcome_at(&crate::vault_root(), checkpoint).await
}

pub(crate) enum ReceiptLookup {
    Absent,
    Pending {
        checkpoint: Option<AgentRunCheckpoint>,
    },
    Conclusive(AgentRunReceipt),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PendingRecovery {
    Recovered,
    Retained(String),
}

/// Read a keyed Agent Run receipt WITHOUT claiming it. Scheduled-fence
/// reconciliation uses this to accept a run whose scheduler died mid-flight:
/// the child persisted its receipt even though no ledger record was appended.
pub(crate) fn lookup_receipt(key: &str) -> io::Result<ReceiptLookup> {
    let dir = crate::state::dir().join("agent-runs");
    let path = idempotency_path(&dir, key)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(ReceiptLookup::Absent),
        Err(error) => return Err(error),
    };
    let record: DurableAgentRun =
        serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let (recorded_key, lookup) = match record {
        DurableAgentRun::InProgress { key, checkpoint } => {
            (key, ReceiptLookup::Pending { checkpoint })
        }
        DurableAgentRun::Succeeded { key, detail } => (
            key,
            ReceiptLookup::Conclusive(AgentRunReceipt::Succeeded { detail }),
        ),
        DurableAgentRun::Failed { key, detail } => (
            key,
            ReceiptLookup::Conclusive(AgentRunReceipt::Failed { detail }),
        ),
    };
    if recorded_key != key {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "idempotency digest collision",
        ));
    }
    Ok(lookup)
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum DurableAgentRun {
    InProgress {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(rename = "identity", alias = "checkpoint")]
        checkpoint: Option<AgentRunCheckpoint>,
    },
    Succeeded {
        key: String,
        detail: String,
    },
    Failed {
        key: String,
        detail: String,
    },
}

enum IdempotencyClaim {
    Execute(PathBuf),
    Prior(AgentRunReceipt),
    Pending,
}

struct ReceiptProgress {
    path: PathBuf,
    key: String,
}

impl AgentRunProgress for ReceiptProgress {
    fn update(&self, checkpoint: AgentRunCheckpoint) -> Result<(), String> {
        persist_agent_checkpoint(&self.path, &self.key, &checkpoint)
            .map_err(|error| format!("persisting Agent Run checkpoint: {error}"))
    }
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
    crate::state::ensure_dir_durable(dir)?;
    let path = idempotency_path(dir, key)?;
    let record = DurableAgentRun::InProgress {
        key: key.to_string(),
        checkpoint: None,
    };
    let mut bytes =
        serde_json::to_vec(&record).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            let result = file
                .write_all(&bytes)
                .and_then(|_| file.sync_all())
                .and_then(|_| crate::state::sync_dir(dir));
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
                DurableAgentRun::InProgress { key, .. } => (key, IdempotencyClaim::Pending),
                DurableAgentRun::Succeeded { key, detail } => (
                    key,
                    IdempotencyClaim::Prior(AgentRunReceipt::Succeeded { detail }),
                ),
                DurableAgentRun::Failed { key, detail } => (
                    key,
                    IdempotencyClaim::Prior(AgentRunReceipt::Failed { detail }),
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

fn persist_agent_outcome(path: &Path, key: &str, outcome: &AgentRunReceipt) -> io::Result<()> {
    if !outcome.durable() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only conclusive outcomes are durable",
        ));
    }
    let record = match outcome {
        AgentRunReceipt::Succeeded { detail } => DurableAgentRun::Succeeded {
            key: key.to_string(),
            detail: detail.clone(),
        },
        AgentRunReceipt::Failed { detail } => DurableAgentRun::Failed {
            key: key.to_string(),
            detail: detail.clone(),
        },
        _ => unreachable!("durability checked above"),
    };
    let mut bytes =
        serde_json::to_vec(&record).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    crate::state::atomic_write(path, &bytes)
}

fn persist_agent_checkpoint(
    path: &Path,
    key: &str,
    checkpoint: &AgentRunCheckpoint,
) -> io::Result<()> {
    let current = serde_json::from_str::<DurableAgentRun>(&std::fs::read_to_string(path)?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    match current {
        DurableAgentRun::InProgress {
            key: current_key,
            checkpoint: Some(current_checkpoint),
        } if current_key == key => {
            if !current_checkpoint.can_follow(checkpoint) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Agent Run checkpoint identity changed",
                ));
            }
            if current_checkpoint.rank() > checkpoint.rank() {
                return Ok(());
            }
        }
        DurableAgentRun::InProgress {
            key: current_key, ..
        } if current_key == key => {}
        DurableAgentRun::Succeeded {
            key: current_key, ..
        }
        | DurableAgentRun::Failed {
            key: current_key, ..
        } if current_key == key => return Ok(()),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Agent Run receipt is no longer an in-progress claim",
            ))
        }
    }
    let record = DurableAgentRun::InProgress {
        key: key.to_string(),
        checkpoint: Some(checkpoint.clone()),
    };
    let mut bytes =
        serde_json::to_vec(&record).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    crate::state::atomic_write(path, &bytes)
}

/// Run an agent at most once for a caller-supplied stable key. Conclusive
/// outcomes are replayed without touching Herdr; uncertain state fails closed.
pub(crate) async fn run_idempotent(agent_run: AgentRun, key: &str) -> AgentRunReceipt {
    let dir = crate::state::dir().join("agent-runs");
    let path = match claim_agent_run(&dir, key) {
        Ok(IdempotencyClaim::Execute(path)) => path,
        Ok(IdempotencyClaim::Prior(outcome)) => return outcome,
        Ok(IdempotencyClaim::Pending) => {
            return AgentRunReceipt::Indeterminate {
                detail: "idempotent agent run has no conclusive outcome; refusing duplicate launch"
                    .to_string(),
            };
        }
        Err(error) => {
            return AgentRunReceipt::Indeterminate {
                detail: format!("idempotency state unavailable: {error}"),
            };
        }
    };

    let progress = ReceiptProgress {
        path: path.clone(),
        key: key.to_string(),
    };
    let outcome = match run_agent_with_progress(agent_run, Some(&progress)).await {
        AgentRunOutcome::Unavailable => {
            if let Err(error) = crate::sweep::release_inflight_owned(&crate::vault_root(), key) {
                return AgentRunReceipt::Indeterminate {
                    detail: format!("could not release unavailable learner batch: {error}"),
                };
            }
            return match std::fs::remove_file(&path).and_then(|_| crate::state::sync_dir(&dir)) {
                Ok(()) => AgentRunReceipt::Retryable {
                    detail: "herdr unavailable before launch".to_string(),
                },
                Err(error) => AgentRunReceipt::Indeterminate {
                    detail: format!("could not release unavailable idempotency claim: {error}"),
                },
            };
        }
        AgentRunOutcome::Succeeded(detail) => AgentRunReceipt::Succeeded { detail },
        AgentRunOutcome::Failed(detail) => AgentRunReceipt::Failed { detail },
    };
    match persist_agent_outcome(&path, key, &outcome) {
        Ok(()) => outcome,
        Err(error) => AgentRunReceipt::Indeterminate {
            detail: format!("could not persist conclusive agent outcome: {error}"),
        },
    }
}

pub(crate) async fn recover_pending(
    key: &str,
    checkpoint: &AgentRunCheckpoint,
) -> io::Result<PendingRecovery> {
    let dir = crate::state::dir().join("agent-runs");
    let path = idempotency_path(&dir, key)?;
    match lookup_receipt(key)? {
        ReceiptLookup::Pending {
            checkpoint: Some(current),
        } if current == *checkpoint => {}
        ReceiptLookup::Pending { .. } => {
            return Ok(PendingRecovery::Retained(
                "pending Agent Run identity changed during recovery".to_string(),
            ))
        }
        _ => {
            return Ok(PendingRecovery::Retained(
                "pending Agent Run receipt is no longer recoverable".to_string(),
            ))
        }
    }
    let progress = ReceiptProgress {
        path: path.clone(),
        key: key.to_string(),
    };
    let outcome = match observe_agent_run(checkpoint, Some(&progress)).await {
        AgentRunObservation::Terminal | AgentRunObservation::Absent => {
            recovered_capture_outcome(checkpoint).await
        }
        AgentRunObservation::Retain(detail) => return Ok(PendingRecovery::Retained(detail)),
    };
    let outcome = match outcome {
        AgentRunRecovery::Succeeded(detail) => AgentRunReceipt::Succeeded { detail },
        AgentRunRecovery::Failed(detail) => AgentRunReceipt::Failed { detail },
        AgentRunRecovery::Retain(detail) => return Ok(PendingRecovery::Retained(detail)),
    };
    persist_agent_outcome(&path, key, &outcome)
        .map(|()| PendingRecovery::Recovered)
        .or_else(|error| {
            Ok(PendingRecovery::Retained(format!(
                "could not persist recovered Agent Run outcome: {error}"
            )))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

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
            &AgentRunReceipt::Succeeded {
                detail: "done once".to_string(),
            },
        )
        .unwrap();
        assert!(matches!(
            claim_agent_run(&dir, "door-batch").unwrap(),
            IdempotencyClaim::Prior(AgentRunReceipt::Succeeded { ref detail })
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

    #[test]
    fn conclusive_receipts_outlive_the_scheduler_that_launched_them() {
        let root =
            std::env::temp_dir().join(format!("plant-receipt-lookup-{}", uuid::Uuid::new_v4()));
        let _state = crate::state::use_test_dir(root.clone());
        let dir = root.join("agent-runs");

        assert!(matches!(
            lookup_receipt("attempt-a").unwrap(),
            ReceiptLookup::Absent
        ));
        let path = match claim_agent_run(&dir, "attempt-a") {
            Ok(IdempotencyClaim::Execute(path)) => path,
            _ => panic!("first claim should execute"),
        };
        // Scheduler supervision ends here; only the Agent Run child remains.
        assert!(matches!(
            lookup_receipt("attempt-a").unwrap(),
            ReceiptLookup::Pending { .. }
        ));
        persist_agent_outcome(
            &path,
            "attempt-a",
            &AgentRunReceipt::Succeeded {
                detail: "agent done".to_string(),
            },
        )
        .unwrap();
        match lookup_receipt("attempt-a").unwrap() {
            ReceiptLookup::Conclusive(receipt) => {
                assert_eq!(receipt.ledger_outcome(), Some(("success", "agent done")))
            }
            _ => panic!("a persisted receipt is conclusive"),
        }

        std::fs::write(&path, "{").unwrap();
        assert!(lookup_receipt("attempt-a").is_err());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pending_receipts_expose_recoverable_execution_identity() {
        let root =
            std::env::temp_dir().join(format!("plant-pending-identity-{}", uuid::Uuid::new_v4()));
        let _state = crate::state::use_test_dir(root.clone());
        let dir = root.join("agent-runs");
        let path = idempotency_path(&dir, "attempt-a").unwrap();
        crate::state::ensure_dir_durable(&dir).unwrap();
        std::fs::write(
            path,
            r#"{"state":"in_progress","key":"attempt-a","identity":{"workspace_id":"w1","pane_id":"w1:p1","terminal_id":"t1","session_id":"s1","stage":"working"}}"#,
        )
        .unwrap();

        let ReceiptLookup::Pending {
            checkpoint: Some(checkpoint),
        } = lookup_receipt("attempt-a").unwrap()
        else {
            panic!("the pending receipt must expose its execution checkpoint")
        };
        assert_eq!(checkpoint.target().workspace_id, "w1");
        assert_eq!(checkpoint.target().pane_id, "w1:p1");
        assert_eq!(checkpoint.session_id(), Some("s1"));
        assert!(matches!(checkpoint, AgentRunCheckpoint::Working { .. }));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capture_completion_requires_a_completed_envelope() {
        let path =
            std::env::temp_dir().join(format!("plant-capture-{}.jsonl", uuid::Uuid::new_v4()));
        std::fs::write(&path, "{\"response\":{\"complete\":false}}\n").unwrap();
        assert!(!capture_has_completed_envelope(&path).unwrap());
        std::fs::write(
            &path,
            "{\"response\":{\"complete\":false}}\n{\"response\":{\"complete\":true}}\n",
        )
        .unwrap();
        assert!(capture_has_completed_envelope(&path).unwrap());
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn recovery_uses_matching_capture_as_terminal_proof() {
        let root =
            std::env::temp_dir().join(format!("plant-recovery-capture-{}", uuid::Uuid::new_v4()));
        let sid = "019f0000-0000-7000-8000-000000000001";
        let session = root.join("2026/08/05").join(sid);
        std::fs::create_dir_all(&session).unwrap();
        std::fs::create_dir_all(root.join(".meta")).unwrap();
        std::fs::write(
            root.join(".meta").join(format!("{sid}.json")),
            r#"{"session_id":"019f0000-0000-7000-8000-000000000001","original_start":"2026-08-05T12:00:00Z"}"#,
        )
        .unwrap();
        let checkpoint: AgentRunCheckpoint = serde_json::from_value(serde_json::json!({
            "stage": "captured",
            "target": {"workspace_id": "w1", "pane_id": "w1:p1"},
            "pane": {"terminal_id": "t1", "session_id": sid}
        }))
        .unwrap();

        std::fs::write(
            session.join("turns.jsonl"),
            "{\"response\":{\"complete\":true}}\n",
        )
        .unwrap();
        assert_eq!(
            recovered_capture_outcome_at(&root, &checkpoint).await,
            AgentRunRecovery::Succeeded(
                "agent completed; recovered from the matching terminal capture".to_string()
            )
        );

        std::fs::write(
            session.join("turns.jsonl"),
            "{\"response\":{\"complete\":false}}\n",
        )
        .unwrap();
        assert!(matches!(
            recovered_capture_outcome_at(&root, &checkpoint).await,
            AgentRunRecovery::Failed(detail) if detail.contains("cannot finish")
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn late_progress_cannot_overwrite_a_recovered_conclusive_receipt() {
        let root =
            std::env::temp_dir().join(format!("plant-progress-order-{}", uuid::Uuid::new_v4()));
        let _state = crate::state::use_test_dir(root.clone());
        let dir = root.join("agent-runs");
        let path = match claim_agent_run(&dir, "attempt-a").unwrap() {
            IdempotencyClaim::Execute(path) => path,
            _ => panic!("the first claim must execute"),
        };
        let working: AgentRunCheckpoint = serde_json::from_value(serde_json::json!({
            "stage": "working",
            "target": {"workspace_id": "w1", "pane_id": "w1:p1"},
            "pane": {"kind": "session_bound", "terminal_id": "t1", "session_id": "s1"}
        }))
        .unwrap();
        persist_agent_checkpoint(&path, "attempt-a", &working).unwrap();
        persist_agent_outcome(
            &path,
            "attempt-a",
            &AgentRunReceipt::Succeeded {
                detail: "recovered".to_string(),
            },
        )
        .unwrap();
        let ready: AgentRunCheckpoint = serde_json::from_value(serde_json::json!({
            "stage": "ready",
            "target": {"workspace_id": "w1", "pane_id": "w1:p1"},
            "pane": {"kind": "session_bound", "terminal_id": "t1", "session_id": "s1"}
        }))
        .unwrap();
        persist_agent_checkpoint(&path, "attempt-a", &ready).unwrap();
        assert!(matches!(
            lookup_receipt("attempt-a").unwrap(),
            ReceiptLookup::Conclusive(AgentRunReceipt::Succeeded { ref detail })
                if detail == "recovered"
        ));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checkpoint_persistence_rejects_identity_replacement() {
        let root = std::env::temp_dir().join(format!(
            "plant-checkpoint-identity-{}",
            uuid::Uuid::new_v4()
        ));
        let _state = crate::state::use_test_dir(root.clone());
        let dir = root.join("agent-runs");
        let path = match claim_agent_run(&dir, "attempt-a").unwrap() {
            IdempotencyClaim::Execute(path) => path,
            _ => panic!("the first claim must execute"),
        };
        let working: AgentRunCheckpoint = serde_json::from_value(serde_json::json!({
            "stage": "working",
            "target": {"workspace_id": "w1", "pane_id": "w1:p1"},
            "pane": {"kind": "session_bound", "terminal_id": "t1", "session_id": "s1"}
        }))
        .unwrap();
        persist_agent_checkpoint(&path, "attempt-a", &working).unwrap();
        let replacement: AgentRunCheckpoint = serde_json::from_value(serde_json::json!({
            "stage": "terminal_observed",
            "target": {"workspace_id": "w1", "pane_id": "w1:p1"},
            "pane": {"kind": "session_bound", "terminal_id": "t1", "session_id": "s2"}
        }))
        .unwrap();
        assert!(persist_agent_checkpoint(&path, "attempt-a", &replacement).is_err());
        let ReceiptLookup::Pending {
            checkpoint: Some(current),
        } = lookup_receipt("attempt-a").unwrap()
        else {
            panic!("the checkpoint must remain pending")
        };
        assert_eq!(current.session_id(), Some("s1"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn receipt_variants_derive_protocol_properties() {
        for (receipt, outcome, code, durable) in [
            (
                AgentRunReceipt::Succeeded {
                    detail: "ok".into(),
                },
                "succeeded",
                0,
                true,
            ),
            (
                AgentRunReceipt::Failed {
                    detail: "bad".into(),
                },
                "failed",
                1,
                true,
            ),
            (
                AgentRunReceipt::UntrackedSucceeded {
                    detail: "ok".into(),
                },
                "untracked_succeeded",
                0,
                false,
            ),
            (
                AgentRunReceipt::UntrackedFailed {
                    detail: "bad".into(),
                },
                "untracked_failed",
                1,
                false,
            ),
            (
                AgentRunReceipt::Retryable {
                    detail: "later".into(),
                },
                "retryable",
                75,
                false,
            ),
            (
                AgentRunReceipt::Indeterminate {
                    detail: "unknown".into(),
                },
                "indeterminate",
                1,
                false,
            ),
        ] {
            assert_eq!(receipt.exit_code(), code);
            assert_eq!(receipt.durable(), durable);
            let json = serde_json::to_value(&receipt).unwrap();
            assert_eq!(json["outcome"], outcome);
            assert!(json.get("durable").is_none());
        }
    }
}
