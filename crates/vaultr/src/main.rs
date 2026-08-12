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
        #[arg(long)]
        update: bool,
        /// Delete existing local indexes before building
        #[arg(long)]
        rebuild: bool,
        /// Decode worker count
        #[arg(long)]
        workers: Option<usize>,
    },
    /// Search the local session index
    Search {
        /// Search terms or a quoted phrase
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
                    update: _,
                    rebuild,
                    workers,
                }) => index(&root, rebuild, workers),
                Cmd::Session(SessionCmd::Search {
                    query,
                    limit,
                    json,
                    no_collapse,
                }) => search(&root, &query.join(" "), limit, json, !no_collapse),
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

fn index(root: &std::path::Path, rebuild: bool, workers: Option<usize>) -> Result<()> {
    let directory = session_index::state_root().join("sessions");
    if rebuild && directory.exists() {
        std::fs::remove_dir_all(&directory)?;
    }
    let stats = session_index::build_session_index(root, workers.unwrap_or(1))?;
    println!(
        "indexed {} sessions and {} turns with {} worker(s)",
        stats.sessions,
        stats.turns,
        workers.unwrap_or(1).max(1),
    );
    Ok(())
}

fn search(
    root: &std::path::Path,
    query: &str,
    limit: usize,
    json: bool,
    collapse: bool,
) -> Result<()> {
    let results = session_index::search_sessions(query, limit, collapse)?;
    let newer_sessions = results.built_at.as_deref().map_or(0, |built_at| {
        vault::list_sessions(root)
            .map(|sessions| {
                sessions
                    .into_iter()
                    .filter(|session| session.meta.last_activity() > built_at)
                    .count()
            })
            .unwrap_or(0)
    });
    let warnings = (results.built_at.is_none() || newer_sessions > 0)
        .then_some("index may be stale; run `vaultr session index --update`");
    if json {
        println!(
            "{}",
            serde_json::json!({
                "query": query,
                "total": results.total,
                "shown": results.hits.len(),
                "freshness": results.built_at,
                "warnings": warnings.iter().collect::<Vec<_>>(),
                "sessions_since_build": newer_sessions,
                "hits": results.hits,
            })
        );
        return Ok(());
    }
    if let Some(warning) = warnings {
        eprintln!("warning: {warning}");
    }
    println!(
        "{} of {} hit(s) shown; built {}; {} sessions captured since build",
        results.hits.len(),
        results.total,
        results.built_at.as_deref().unwrap_or("-"),
        newer_sessions,
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
        println!(
            "{}",
            hit.body.lines().take(3).collect::<Vec<_>>().join("\n")
        );
    }
    Ok(())
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
