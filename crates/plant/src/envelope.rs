//! Vault capture-envelope evidence.
//!
//! A leaf over `vaultr`: it answers whether a captured session has reached a
//! completed response envelope. Both the Herdr lifecycle and the Agent Run
//! coordinator need that answer, so it lives below both of them.

use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};

pub(crate) fn capture_has_completed_envelope(path: &Path) -> Result<bool, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
