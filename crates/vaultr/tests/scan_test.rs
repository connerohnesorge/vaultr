use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::tempdir;

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn scan(repo: &Path, range: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vaultr"))
        .arg("scan")
        .arg("--repo")
        .arg(repo)
        .arg("--range")
        .arg(range)
        .arg("--no-review")
        .output()
        .unwrap()
}

#[test]
fn scan_reads_only_the_requested_committed_range() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir(&repo).unwrap();

    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "vaultr test"]);
    git(&repo, &["config", "user.email", "vaultr@example.com"]);
    fs::write(repo.join(".secretsignore"), b"\n").unwrap();
    fs::write(repo.join("README.md"), b"seed\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "seed"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);

    fs::write(repo.join("clean.md"), b"safe content\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "clean"]);
    let clean = git(&repo, &["rev-parse", "HEAD"]);

    fs::write(
        repo.join("working.env"),
        b"GITLAB_TOKEN=glpat-3Kd9Vq2ZmXr7Lb1TnWpA\n",
    )
    .unwrap();
    let output = scan(&repo, &format!("{base}..{clean}"));
    assert!(
        output.status.success(),
        "clean scan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "secret scan clean"
    );

    fs::rename(repo.join("working.env"), repo.join("leak.env")).unwrap();
    git(&repo, &["add", "leak.env"]);
    git(&repo, &["commit", "-qm", "leak"]);
    let output = scan(&repo, &format!("{clean}..HEAD"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout.contains("secret finding: leak.env:"), "{stdout}");
    assert!(
        !stdout.contains("3Kd9Vq2"),
        "scanner echoed the secret: {stdout}"
    );
}
