//! `vaultr session fork` orchestration: reconstruct a captured session from
//! the vault (never from native agent stores) and write it as a fresh native
//! session for Claude Code or Codex, then launch the target CLI on it.
//!
//! Same-harness forks pass the reconstructed raw wire history through with
//! minimal transformation (the cacheability gate requires the resumed messages
//! array to be byte-identical to a native resume). Cross-harness forks go
//! through the normalized model plus best-effort tool translation.

use crate::recon::Harness;
use crate::{claude_writer, codex_writer, normalize, recon, translate, vault};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Target {
    Claude,
    Codex,
}

#[derive(Debug, Default)]
pub struct ForkOptions {
    /// Override the launch cwd (default: the session's recorded cwd).
    pub cwd: Option<PathBuf>,
    /// Skip launching the target CLI; print the command instead.
    pub no_launch: bool,
    /// Override CLAUDE_CONFIG_DIR resolution (tests).
    pub claude_config_dir: Option<PathBuf>,
    /// Override CODEX_HOME resolution (tests).
    pub codex_home: Option<PathBuf>,
}

#[derive(Debug)]
pub struct ForkOutcome {
    pub new_id: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    /// The launch command, e.g. ["claude", "--resume", "<id>"].
    pub launch: Vec<String>,
}

/// Fork a captured session into a fresh native session file.
pub fn fork(root: &Path, id: &str, target: Target, opts: &ForkOptions) -> Result<ForkOutcome> {
    let session = vault::resolve_id(root, id)?;
    let dir = vault::session_dir(root, &session)?;
    let capture = vault::capture_file(&dir)?;
    let recon = recon::reconstruct(&capture)?;
    if recon.messages.is_empty() {
        bail!("session {} reconstructed to an empty history", session.id);
    }
    // Recon's envelope-derived identity is authoritative: envelopes are the
    // captured wire truth, while .meta/<id>.json is a mutable hook-merged
    // sidecar that can go stale. meta.harness is consulted only for degenerate
    // captures where neither the envelope field nor the history key resolved.
    let source = recon.harness.unwrap_or_else(|| {
        session
            .meta
            .harness
            .as_deref()
            .and_then(Harness::from_label)
            .unwrap_or(Harness::Claude)
    });

    // Resolve and validate the launch cwd BEFORE any write.
    let cwd =
        match &opts.cwd {
            Some(p) => p.clone(),
            None => PathBuf::from(session.meta.cwd.as_deref().with_context(|| {
                format!("session {} has no recorded cwd; pass --cwd", session.id)
            })?),
        };
    if !cwd.is_dir() {
        bail!(
            "target cwd {} does not exist (nothing was written)",
            cwd.display()
        );
    }
    let cwd_str = cwd.to_string_lossy().to_string();
    let git_branch = session.meta.git_branch.as_deref();

    let (new_id, path, launch) = match target {
        Target::Claude => {
            let messages: Vec<Value> = if source == Harness::Claude {
                recon.messages.clone()
            } else {
                translate::to_anthropic(&normalize::normalize(&recon.messages))
            };
            let (id, path) = claude_writer::write(
                &claude_config_dir(opts),
                &cwd_str,
                git_branch,
                session.meta.model.as_deref(),
                &messages,
            )?;
            let launch = vec!["claude".into(), "--resume".into(), id.clone()];
            (id, path, launch)
        }
        Target::Codex => {
            let (items, base_instructions) = if source == Harness::Codex {
                prepare_codex_passthrough(&recon.messages)
            } else {
                (
                    translate::to_codex(&normalize::normalize(&recon.messages)),
                    None,
                )
            };
            let (id, path) = codex_writer::write(
                &codex_home(opts),
                &cwd_str,
                git_branch,
                &items,
                base_instructions.as_deref(),
                session.meta.model.as_deref(),
            )?;
            let launch = vec!["codex".into(), "resume".into(), id.clone()];
            (id, path, launch)
        }
    };

    Ok(ForkOutcome {
        new_id,
        path,
        cwd,
        launch,
    })
}

/// Replace the current process with the target CLI in the fork's cwd.
pub fn launch(outcome: &ForkOutcome) -> Result<()> {
    let mut cmd = std::process::Command::new(&outcome.launch[0]);
    cmd.args(&outcome.launch[1..]).current_dir(&outcome.cwd);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(err).with_context(|| format!("exec {}", outcome.launch.join(" ")))
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status()?;
        if !status.success() {
            bail!("{} exited with {status}", outcome.launch.join(" "));
        }
        Ok(())
    }
}

fn claude_config_dir(opts: &ForkOptions) -> PathBuf {
    if let Some(p) = &opts.claude_config_dir {
        return p.clone();
    }
    if let Ok(v) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    home().join(".claude")
}

fn codex_home(opts: &ForkOptions) -> PathBuf {
    if let Some(p) = &opts.codex_home {
        return p.clone();
    }
    if let Ok(v) = std::env::var("CODEX_HOME") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    home().join(".codex")
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

/// Prepare a Codex->Codex passthrough: Codex regenerates the per-request
/// scaffolding it never records — the `additional_tools` item and the leading
/// base-instructions developer message. Drop those and lift the instructions
/// text into session_meta.base_instructions, exactly as real rollouts do;
/// everything else (including reasoning items with encrypted_content) passes
/// through opaquely.
pub fn prepare_codex_passthrough(messages: &[Value]) -> (Vec<Value>, Option<String>) {
    let mut base_instructions: Option<String> = None;
    let mut items: Vec<Value> = Vec::new();
    let mut dropped_dev = false;
    for m in messages {
        let ty = m.get("type").and_then(Value::as_str);
        if ty == Some("additional_tools") {
            continue;
        }
        if !dropped_dev
            && ty == Some("message")
            && m.get("role").and_then(Value::as_str) == Some("developer")
        {
            dropped_dev = true;
            base_instructions = m
                .get("content")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|b| b.get("text"))
                .and_then(Value::as_str)
                .map(String::from);
            continue;
        }
        items.push(m.clone());
    }
    (items, base_instructions)
}
