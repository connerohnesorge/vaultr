use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use vaultr::{fork, normalize, recon, render, scan, seals, session_index, validate, vault};

#[derive(Parser)]
#[command(
    name = "vaultr",
    about = "CLI over captured agent-session wire data",
    version
)]
struct Cli {
    /// Sessions root override (default: $VAULT_SESSIONS or ~/.dotfiles/vault/sessions)
    #[arg(long, global = true)]
    vault: Option<PathBuf>,
    /// Fail on a local miss instead of fetching the seal from the S3 store
    #[arg(long, global = true)]
    no_fetch: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Session operations
    #[command(subcommand)]
    Session(SessionCmd),
    /// Validate vault content: wikilink resolution, frontmatter, md paths, ledger
    Validate {
        /// Emit the report as JSON
        #[arg(long)]
        json: bool,
        /// Also fail (exit 1) on warnings
        #[arg(long)]
        strict: bool,
    },
    /// Scan committed text blobs for secrets
    Scan(scan::ScanArgs),
}

#[derive(Subcommand)]
enum SessionCmd {
    /// List captured sessions (current cwd only by default)
    List {
        /// Show sessions from all working directories
        #[arg(long)]
        all: bool,
    },
    /// Print the session directory path
    Path {
        id: String,
        /// Also copy the path to the clipboard (pbcopy)
        #[arg(long)]
        copy: bool,
    },
    /// Render the session transcript as Markdown
    Show {
        id: String,
        /// Print reconstruction stats instead of the transcript
        #[arg(long, hide = true)]
        stats: bool,
    },
    /// Build or update local session-search indexes
    Index {
        /// Update the local indexes incrementally
        #[arg(long, conflicts_with = "rebuild")]
        update: bool,
        /// Delete existing local indexes before building
        #[arg(long, conflicts_with = "update")]
        rebuild: bool,
        /// Decode worker count
        #[arg(long)]
        workers: Option<usize>,
    },
    /// Search the local session index
    Search {
        /// Search terms or a quoted phrase
        #[arg(required = true)]
        query: Vec<String>,
        /// Maximum result count
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Emit result records as JSON
        #[arg(long)]
        json: bool,
        /// Return duplicate turn bodies
        #[arg(long)]
        no_collapse: bool,
        /// Exclude turns absent from the final replay
        #[arg(long)]
        final_only: bool,
        /// Include up to three curated vault records
        #[arg(long)]
        curated: bool,
    },
    /// Fork a captured session into a fresh native Claude/Codex/Pi session
    Fork {
        id: String,
        /// Target harness to fork into
        #[arg(long, value_enum)]
        into: fork::Target,
        /// Launch cwd override (default: the session's recorded cwd)
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Write the session file but do not launch the target CLI
        #[arg(long)]
        no_launch: bool,
        /// Submit an initial prompt after resuming the target session
        #[arg(long)]
        prompt: Option<String>,
        /// Restrict the resumed target to native read-only controls
        #[arg(long)]
        read_only: bool,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Scan(args) => {
            let code = scan::run(args)?;
            std::process::exit(code);
        }
        command => {
            let root = vault::root(cli.vault.as_deref())?;
            match command {
                Cmd::Validate { json, strict } => {
                    let code = validate::run(&root, json, strict)?;
                    std::process::exit(code);
                }
                Cmd::Session(SessionCmd::List { all }) => list(&root, all),
                Cmd::Session(SessionCmd::Path { id, copy }) => {
                    path(&root, &id, copy, !cli.no_fetch)
                }
                Cmd::Session(SessionCmd::Show { id, stats }) => {
                    show(&root, &id, stats, !cli.no_fetch)
                }
                Cmd::Session(SessionCmd::Index {
                    update,
                    rebuild,
                    workers,
                }) => index(&root, update, rebuild, workers),
                Cmd::Session(SessionCmd::Search {
                    query,
                    limit,
                    json,
                    no_collapse,
                    final_only,
                    curated,
                }) => search(
                    &root,
                    &query.join(" "),
                    session_index::SearchOptions {
                        limit,
                        collapse: !no_collapse,
                        final_only,
                        curated,
                    },
                    json,
                ),
                Cmd::Session(SessionCmd::Fork {
                    id,
                    into,
                    cwd,
                    no_launch,
                    prompt,
                    read_only,
                }) => {
                    let opts = fork::ForkOptions {
                        cwd,
                        no_launch,
                        prompt,
                        read_only,
                        no_fetch: cli.no_fetch,
                        ..Default::default()
                    };
                    let outcome = fork::fork(&root, &id, into, &opts)?;
                    eprintln!("forked {} -> {}", id, outcome.path.display());
                    if no_launch {
                        println!(
                            "launch with: (cd {} && {})",
                            outcome.cwd.display(),
                            outcome.launch.join(" ")
                        );
                        Ok(())
                    } else {
                        fork::launch(&outcome)
                    }
                }
                Cmd::Scan(_) => unreachable!("scan is handled before the session root is resolved"),
            }
        }
    }
}

fn index(
    root: &std::path::Path,
    update: bool,
    rebuild: bool,
    workers: Option<usize>,
) -> Result<()> {
    if !update && !rebuild {
        anyhow::bail!("choose `--update` or `--rebuild`");
    }
    let stats = session_index::update_indexes(root, workers.unwrap_or(1), rebuild)?;
    println!(
        "indexed {} sessions and {} turns ({} changed); {} curated records ({} changed) with {} worker(s)",
        stats.sessions,
        stats.turns,
        stats.changed_sessions,
        stats.curated_documents,
        stats.changed_curated_documents,
        stats.workers,
    );
    Ok(())
}

fn search(
    root: &std::path::Path,
    query: &str,
    options: session_index::SearchOptions,
    json: bool,
) -> Result<()> {
    let results = session_index::search(query, &options)?;
    let freshness = index_freshness(results.built_at.as_deref());
    let newer_sessions = if let Some(freshness) = freshness.as_deref() {
        vault::list_sessions(root)?
            .into_iter()
            .filter(|session| timestamp_after(session.meta.last_activity(), freshness))
            .count()
    } else {
        0
    };
    let warnings = if freshness.is_none() || newer_sessions > 0 {
        vec!["index may be stale; run `vaultr session index --update`"]
    } else {
        Vec::new()
    };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "query": query,
                "total": results.total,
                "shown": results.hits.len(),
                "curated_total": results.curated_total,
                "curated_shown": results.curated_hits.len(),
                "freshness": freshness,
                "warnings": warnings,
                "sessions_since_build": newer_sessions,
                "coverage": results.coverage,
                "curated_hits": results.curated_hits,
                "hits": results.hits,
            })
        );
        return Ok(());
    }
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    if options.curated {
        println!(
            "{} of {} curated hit(s) shown",
            results.curated_hits.len(),
            results.curated_total,
        );
        for hit in results.curated_hits {
            println!("{} | score={:.3}", hit.path, hit.score);
            println!("{}", hit.snippet);
        }
    }
    println!(
        "{} of {} session hit(s) shown; built {}; {} sessions captured since build",
        results.hits.len(),
        results.total,
        freshness.as_deref().unwrap_or("-"),
        newer_sessions,
    );
    println!(
        "metadata coverage: harness={}/{}, cwd={}/{}, branch={}/{}, model={}/{}, timestamp={}/{}",
        results.coverage.harness,
        results.coverage.sessions,
        results.coverage.cwd,
        results.coverage.sessions,
        results.coverage.branch,
        results.coverage.sessions,
        results.coverage.model,
        results.coverage.sessions,
        results.coverage.timestamp,
        results.coverage.sessions,
    );
    for hit in results.hits {
        let markers = [
            hit.compacted.then(|| "compacted".to_string()),
            hit.partial.then(|| "partial".to_string()),
            (hit.duplicates > 1).then(|| format!("{} duplicates", hit.duplicates)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        println!(
            "{} | {} | {} | cwd={} | branch={} | turn={}{}",
            &hit.session_id[..hit.session_id.len().min(8)],
            if hit.timestamp.is_empty() {
                "-"
            } else {
                &hit.timestamp
            },
            if hit.harness.is_empty() {
                "-"
            } else {
                &hit.harness
            },
            if hit.cwd.is_empty() { "-" } else { &hit.cwd },
            if hit.branch.is_empty() {
                "-"
            } else {
                &hit.branch
            },
            hit.turn_index,
            if markers.is_empty() {
                String::new()
            } else {
                format!(" | {markers}")
            },
        );
        println!("{}", hit.snippet);
    }
    Ok(())
}

fn timestamp_after(activity: &str, built_at: &str) -> bool {
    match (
        chrono::DateTime::parse_from_rfc3339(activity),
        chrono::DateTime::parse_from_rfc3339(built_at),
    ) {
        (Ok(activity), Ok(built_at)) => activity > built_at,
        _ => !activity.is_empty() && activity > built_at,
    }
}

fn index_freshness(built_at: Option<&str>) -> Option<String> {
    let ledger = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".local/state/plant/jobs/session-index.jsonl"));
    let ledger_timestamp = ledger
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| {
            text.lines()
                .rev()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find(|record| {
                    record.get("outcome").and_then(serde_json::Value::as_str) == Some("success")
                })
                .and_then(|record| record.get("ts").and_then(serde_json::Value::as_i64))
        })
        .and_then(|timestamp| chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0))
        .map(|timestamp| timestamp.to_rfc3339());
    match (built_at, ledger_timestamp) {
        (Some(built_at), Some(ledger)) if timestamp_after(&ledger, built_at) => Some(ledger),
        (Some(built_at), _) => Some(built_at.to_string()),
        (None, ledger) => ledger,
    }
}

fn list(root: &std::path::Path, all: bool) -> Result<()> {
    let mut sessions = vault::list_sessions(root)?;
    if !all {
        let raw_cwd = std::env::current_dir()?;
        let cwd = raw_cwd
            .canonicalize()
            .unwrap_or(raw_cwd)
            .to_string_lossy()
            .to_string();
        sessions.retain(|s| {
            s.meta.cwd.as_deref().is_some_and(|c| {
                PathBuf::from(c)
                    .canonicalize()
                    .map(|p| p.to_string_lossy() == cwd)
                    .unwrap_or(c == cwd)
            })
        });
    }
    if sessions.is_empty() {
        if all {
            println!("no sessions");
        } else {
            println!("no sessions for this cwd (try --all)");
        }
        return Ok(());
    }
    println!(
        "{:<38} {:<12} {:<24} {:<26} CWD",
        "ID", "HARNESS", "MODEL", "LAST ACTIVITY"
    );
    for s in &sessions {
        println!(
            "{:<38} {:<12} {:<24} {:<26} {}",
            s.id,
            s.meta.harness.as_deref().unwrap_or("-"),
            s.meta.model.as_deref().unwrap_or("-"),
            s.meta.last_activity(),
            s.meta.cwd.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

/// A path is only useful if something is at it, so a local miss fetches here too
/// rather than printing a directory that does not exist.
fn path(root: &std::path::Path, id: &str, copy: bool, fetch: bool) -> Result<()> {
    let session = vault::resolve_id(root, id)?;
    let dir = seals::materialise(root, &session, fetch)?.dir;
    let text = dir.display().to_string();
    println!("{text}");
    if copy {
        let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn()?;
        use std::io::Write;
        child.stdin.take().unwrap().write_all(text.as_bytes())?;
        child.wait()?;
    }
    Ok(())
}

fn show(root: &std::path::Path, id: &str, stats: bool, fetch: bool) -> Result<()> {
    let session = vault::resolve_id(root, id)?;
    let file = seals::materialise(root, &session, fetch)?.capture;
    let recon = recon::reconstruct(&file)?;
    if stats {
        println!(
            "{}",
            serde_json::json!({
                "key": recon.key,
                "n": recon.history_len,
                "trailing_appended": recon.trailing_appended,
                "envelopes": recon.envelopes,
                "total": recon.messages.len(),
            })
        );
        return Ok(());
    }
    let normalized = normalize::normalize(&recon.messages);
    print!("{}", render::markdown(&normalized));
    Ok(())
}
