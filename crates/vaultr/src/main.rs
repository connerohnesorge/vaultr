use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use vaultr::{fork, normalize, recon, render, validate, vault};

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
    /// Fork a captured session into a fresh native Claude/Codex session
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
    let root = vault::root(cli.vault.as_deref())?;
    match cli.command {
        Cmd::Validate { json, strict } => {
            let code = validate::run(&root, json, strict)?;
            std::process::exit(code);
        }
        Cmd::Session(SessionCmd::List { all }) => list(&root, all),
        Cmd::Session(SessionCmd::Path { id, copy }) => path(&root, &id, copy),
        Cmd::Session(SessionCmd::Show { id, stats }) => show(&root, &id, stats),
        Cmd::Session(SessionCmd::Fork {
            id,
            into,
            cwd,
            no_launch,
        }) => {
            let opts = fork::ForkOptions {
                cwd,
                no_launch,
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

fn path(root: &std::path::Path, id: &str, copy: bool) -> Result<()> {
    let session = vault::resolve_id(root, id)?;
    let dir = vault::session_dir(root, &session)?;
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

fn show(root: &std::path::Path, id: &str, stats: bool) -> Result<()> {
    let session = vault::resolve_id(root, id)?;
    let dir = vault::session_dir(root, &session)?;
    let file = vault::capture_file(&dir)?;
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
