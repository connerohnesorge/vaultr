mod engine;
mod input;
mod review;
mod server;

use anyhow::Result;
use clap::Args;
use std::io::IsTerminal;
use std::path::PathBuf;

use crate::secrets;

#[derive(Debug, Args)]
pub struct ScanArgs {
    /// Repository whose committed range should be scanned
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    /// Revision range to inspect
    #[arg(long, default_value = "origin/main..HEAD")]
    pub range: String,
    /// Restrict the scan to these changed repository-relative paths
    #[arg(long, value_name = "PATH", num_args = 1..)]
    pub paths: Vec<PathBuf>,
    /// Emit the validate-compatible report as JSON
    #[arg(long)]
    pub json: bool,
    /// Always open the local review page when findings exist
    #[arg(long, conflicts_with = "no_review")]
    pub review: bool,
    /// Never open a browser or wait for review input
    #[arg(long = "no-review", conflicts_with = "review")]
    pub no_review: bool,
}

fn print_findings(findings: &[engine::ScannedFinding]) {
    for finding in findings {
        println!(
            "secret finding: {}:{}:{}",
            finding.path.display(),
            finding.hit.line,
            finding.hit.rule
        );
    }
}

fn print_report(report: &crate::validate::Report) -> Result<()> {
    println!("{}", serde_json::to_string(report)?);
    Ok(())
}

pub fn run(args: ScanArgs) -> Result<i32> {
    let input = match input::build(&args) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("scan failed: {error:#}");
            return Ok(2);
        }
    };
    let mut policy = match secrets::policy_for(&input.repo) {
        Ok(policy) => policy,
        Err(error) => {
            eprintln!("scan failed: {error:#}");
            return Ok(2);
        }
    };
    let result = match engine::scan(&input, &policy) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("scan failed: {error:#}");
            return Ok(2);
        }
    };
    if args.json {
        if let Err(error) = print_report(&result.report) {
            eprintln!("scan failed: {error:#}");
            return Ok(2);
        }
        return Ok(i32::from(!result.findings.is_empty()));
    }
    if result.findings.is_empty() {
        println!("secret scan clean");
        return Ok(0);
    }
    let review = args.review || (!args.no_review && std::io::stdout().is_terminal());
    if !review {
        print_findings(&result.findings);
        return Ok(1);
    }
    match server::serve_review(&input, &mut policy, result.findings) {
        Ok(code) => Ok(code),
        Err(error) => {
            eprintln!("scan failed: {error:#}");
            Ok(2)
        }
    }
}
