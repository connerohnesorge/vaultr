use crate::herdr::{run_agent, AgentRun, AgentRunOutcome};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

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

pub(crate) enum ReceiptLookup {
    Absent,
    Pending,
    Conclusive(AgentRunReceipt),
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
        DurableAgentRun::InProgress { key } => (key, ReceiptLookup::Pending),
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
    InProgress { key: String },
    Succeeded { key: String, detail: String },
    Failed { key: String, detail: String },
}

enum IdempotencyClaim {
    Execute(PathBuf),
    Prior(AgentRunReceipt),
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
    crate::state::ensure_dir_durable(dir)?;
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
                DurableAgentRun::InProgress { key } => (key, IdempotencyClaim::Pending),
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

    let outcome = match run_agent(agent_run).await {
        AgentRunOutcome::Unavailable => {
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
            ReceiptLookup::Pending
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
