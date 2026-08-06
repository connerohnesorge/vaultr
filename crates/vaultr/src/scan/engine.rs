use anyhow::Result;
use std::ops::Range;

use crate::secrets::{self, Hit, Policy};
use crate::validate::{Finding, Report, Severity};

use super::input::{self, ScanInput};

pub(super) struct ScannedFinding {
    pub(super) path: std::path::PathBuf,
    pub(super) hit: Hit,
    pub(super) line: String,
    pub(super) line_span: Range<usize>,
    pub(super) matched: Vec<u8>,
}

pub(super) struct ScanResult {
    pub(super) report: Report,
    pub(super) findings: Vec<ScannedFinding>,
}

fn line_bounds(bytes: &[u8], offset: usize) -> (usize, usize) {
    let offset = offset.min(bytes.len());
    let start = bytes[..offset]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |newline| newline + 1);
    let end = bytes[offset..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |newline| offset + newline);
    (start, end)
}

pub(super) fn scan(input: &ScanInput, policy: &Policy) -> Result<ScanResult> {
    let mut report = Report {
        files: input.paths.len(),
        links: 0,
        findings: Vec::new(),
    };
    let mut findings = Vec::new();
    for path in &input.paths {
        let bytes = input::read_blob(input, path)?;
        for hit in secrets::scan_bytes(&bytes, path, policy) {
            let (start, end) = line_bounds(&bytes, hit.span.start);
            let line = String::from_utf8_lossy(&bytes[start..end]).into_owned();
            let line_span = hit.span.start - start..hit.span.end - start;
            report.findings.push(Finding {
                severity: Severity::Error,
                kind: hit.rule,
                file: path.to_string_lossy().into_owned(),
                line: hit.line,
                detail: "secret detected".into(),
            });
            findings.push(ScannedFinding {
                path: path.clone(),
                hit: hit.clone(),
                line,
                line_span,
                matched: bytes[hit.span].to_vec(),
            });
        }
    }
    Ok(ScanResult { report, findings })
}
