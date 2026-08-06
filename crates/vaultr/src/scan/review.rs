use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::collections::HashMap;

use crate::secrets::{self, Policy};

use super::engine::{self, ScannedFinding};
use super::input::ScanInput;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Decision {
    Real,
    FalsePositive,
}

#[derive(Serialize)]
pub(super) struct ReviewFinding {
    file: String,
    line: usize,
    rule: &'static str,
    text: String,
    span_start: usize,
    span_end: usize,
    decision: Option<&'static str>,
}

pub(super) struct ReviewState {
    findings: Vec<ScannedFinding>,
    decisions: HashMap<String, Decision>,
}

fn review_key(finding: &ScannedFinding) -> String {
    format!(
        "{}:{}:{}:{}",
        finding.path.display(),
        finding.hit.line,
        finding.hit.rule,
        secrets::digest_for_review(&finding.matched)
    )
}

fn decision_name(decision: Option<Decision>) -> Option<&'static str> {
    match decision {
        Some(Decision::Real) => Some("real"),
        Some(Decision::FalsePositive) => Some("false-positive"),
        None => None,
    }
}

impl ReviewState {
    pub(super) fn new(findings: Vec<ScannedFinding>) -> Self {
        Self {
            findings,
            decisions: HashMap::new(),
        }
    }

    pub(super) fn view(&self) -> Vec<ReviewFinding> {
        self.findings
            .iter()
            .map(|finding| ReviewFinding {
                file: finding.path.to_string_lossy().into_owned(),
                line: finding.hit.line,
                rule: finding.hit.rule,
                text: finding.line.clone(),
                span_start: finding.line_span.start,
                span_end: finding.line_span.end,
                decision: decision_name(self.decisions.get(&review_key(finding)).copied()),
            })
            .collect()
    }

    fn update(&mut self, findings: Vec<ScannedFinding>) {
        self.findings = findings;
    }

    fn mark(&mut self, index: usize, decision: Decision) -> Result<()> {
        let finding = self
            .findings
            .get(index)
            .ok_or_else(|| anyhow!("finding index is out of range"))?;
        self.decisions.insert(review_key(finding), decision);
        Ok(())
    }

    fn decision(&self, finding: &ScannedFinding) -> Option<Decision> {
        self.decisions.get(&review_key(finding)).copied()
    }
}

pub(super) fn action(
    input: &ScanInput,
    policy: &mut Policy,
    state: &mut ReviewState,
    body: &[u8],
) -> Result<(String, Option<i32>)> {
    let action: serde_json::Value = serde_json::from_slice(body).context("parse review action")?;
    match action.get("action").and_then(serde_json::Value::as_str) {
        Some("judge") => {
            let index = action
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| anyhow!("review action has no index"))?
                as usize;
            match action.get("decision").and_then(serde_json::Value::as_str) {
                Some("real") => state.mark(index, Decision::Real)?,
                Some("false-positive") => {
                    let finding = state
                        .findings
                        .get(index)
                        .ok_or_else(|| anyhow!("finding index is out of range"))?;
                    secrets::allow_false_positive(
                        &input.repo,
                        &finding.path,
                        finding.hit.rule,
                        &finding.matched,
                        "Marked false positive in local review",
                        &chrono::Utc::now().date_naive().to_string(),
                        policy,
                    )?;
                    state.mark(index, Decision::FalsePositive)?;
                }
                _ => return Err(anyhow!("review decision must be real or false-positive")),
            }
            Ok((r#"{"ok":true}"#.into(), None))
        }
        Some("done") => {
            let refreshed = engine::scan(input, policy)?;
            state.update(refreshed.findings);
            if state.findings.is_empty() {
                return Ok((r#"{"ok":true,"exit":0,"message":"clean"}"#.into(), Some(0)));
            }
            if state
                .findings
                .iter()
                .all(|finding| state.decision(finding) == Some(Decision::Real))
            {
                return Ok((
                    r#"{"ok":true,"exit":1,"message":"real findings remain"}"#.into(),
                    Some(1),
                ));
            }
            Ok((
                r#"{"ok":false,"message":"judge every finding, then press Done again"}"#.into(),
                None,
            ))
        }
        _ => Err(anyhow!("unknown review action")),
    }
}
