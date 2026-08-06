use anyhow::{anyhow, Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::ScanArgs;

pub(super) struct ScanInput {
    pub(super) repo: PathBuf,
    pub(super) revision: String,
    pub(super) paths: Vec<PathBuf>,
}

fn git_output(repo: &Path, args: &[String]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("run git in {}", repo.display()))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(anyhow!(
            "git {} failed{}",
            args.first().map(String::as_str).unwrap_or("command"),
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        ));
    }
    Ok(output.stdout)
}

fn normalize_path(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

fn right_revision(range: &str) -> Result<&str> {
    range
        .rsplit_once("...")
        .or_else(|| range.rsplit_once(".."))
        .map(|(_, right)| right)
        .filter(|right| !right.is_empty())
        .ok_or_else(|| anyhow!("--range must contain a right-hand revision"))
}

pub(super) fn build(args: &ScanArgs) -> Result<ScanInput> {
    let repo = args
        .repo
        .canonicalize()
        .with_context(|| format!("resolve --repo {}", args.repo.display()))?;
    if !repo.is_dir() {
        return Err(anyhow!("--repo is not a directory: {}", repo.display()));
    }
    let diff = git_output(
        &repo,
        &[
            "diff".into(),
            "-z".into(),
            "--name-only".into(),
            "--diff-filter=ACMR".into(),
            args.range.clone(),
        ],
    )?;
    let mut changed = Vec::new();
    for raw in diff
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
    {
        let path = std::str::from_utf8(raw)
            .context("git returned a non-UTF-8 path")
            .and_then(|path| {
                normalize_path(Path::new(path)).ok_or_else(|| anyhow!("unsafe git path: {path}"))
            })?;
        changed.push(path);
    }
    let requested: Option<HashSet<PathBuf>> = if args.paths.is_empty() {
        None
    } else {
        Some(
            args.paths
                .iter()
                .map(|path| {
                    normalize_path(path).ok_or_else(|| {
                        anyhow!("--paths must be repository-relative: {}", path.display())
                    })
                })
                .collect::<Result<_>>()?,
        )
    };
    changed.retain(|path| requested.as_ref().is_none_or(|paths| paths.contains(path)));
    let right = right_revision(&args.range)?;
    let revision = String::from_utf8(git_output(
        &repo,
        &[
            "rev-parse".into(),
            "--verify".into(),
            format!("{right}^{{commit}}"),
        ],
    )?)
    .context("git returned a non-UTF-8 revision")?
    .trim()
    .to_owned();
    Ok(ScanInput {
        repo,
        revision,
        paths: changed,
    })
}

pub(super) fn read_blob(input: &ScanInput, path: &Path) -> Result<Vec<u8>> {
    let object = format!("{}:{}", input.revision, path.to_string_lossy());
    let output = Command::new("git")
        .arg("-C")
        .arg(&input.repo)
        .arg("show")
        .arg(&object)
        .output()
        .with_context(|| format!("read committed blob {path:?}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "could not extract committed bytes for: {}",
            path.display()
        ));
    }
    Ok(output.stdout)
}
