#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

const ID: &str = "019ff5cf-68af-704d-881f-cc1af66fe382";
const DATE: &str = "2026/08/12";

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    session: PathBuf,
    bin: PathBuf,
    log: PathBuf,
    object: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let session = root.join(DATE).join(ID);
        let bin = temp.path().join("bin");
        let log = temp.path().join("aws.log");
        let object = temp.path().join("herdr-object.zst");
        fs::create_dir_all(root.join(".meta")).unwrap();
        fs::create_dir_all(&session).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            root.join(".meta").join(format!("{ID}.json")),
            r#"{"original_start":"2026-08-12T12:00:00Z"}"#,
        )
        .unwrap();
        let aws = bin.join("aws");
        fs::write(
            &aws,
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$AWS_LOG"
case "$1 $2" in
  "s3api head-object")
    case "$AWS_MODE" in
      missing) echo 'An error occurred (404) when calling HeadObject: Not Found' >&2; exit 255 ;;
      denied) echo 'An error occurred (403) when calling HeadObject: AccessDenied' >&2; exit 255 ;;
      success) printf '{"ContentLength":%s}\n' "$AWS_SIZE" ;;
      *) echo "unknown AWS_MODE=$AWS_MODE" >&2; exit 2 ;;
    esac
    ;;
  "s3 cp")
    for arg in "$@"; do destination=$arg; done
    cp "$AWS_OBJECT" "$destination"
    ;;
  *) echo "unexpected aws invocation: $*" >&2; exit 2 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&aws, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _temp: temp,
            root,
            session,
            bin,
            log,
            object,
        }
    }

    fn run(&self, mode: &str, no_fetch: bool) -> Output {
        let path = std::env::join_paths(std::iter::once(self.bin.clone()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        let size = fs::metadata(&self.object).map(|m| m.len()).unwrap_or(0);
        let mut command = Command::new(env!("CARGO_BIN_EXE_vaultr"));
        command
            .arg("--vault")
            .arg(&self.root)
            .args(["session", "herdr", ID])
            .env("PATH", path)
            .env("VAULTR_SEAL_STORE", "s3://test-seals")
            .env("AWS_MODE", mode)
            .env("AWS_SIZE", size.to_string())
            .env("AWS_OBJECT", &self.object)
            .env("AWS_LOG", &self.log);
        if no_fetch {
            command.arg("--no-fetch");
        }
        command.output().unwrap()
    }

    fn write_object(&self, body: &[u8]) -> Vec<u8> {
        let compressed = zstd::stream::encode_all(body, 0).unwrap();
        fs::write(&self.object, &compressed).unwrap();
        compressed
    }

    fn aws_log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

#[test]
fn local_raw_then_sealed_sidecar_never_reaches_aws() {
    let fixture = Fixture::new();
    let raw = b"{\"pane\":\"raw\"}\n";
    let sealed = b"{\"pane\":\"sealed\"}\n";
    fs::write(fixture.session.join("herdr.jsonl"), raw).unwrap();
    fs::write(
        fixture.session.join("herdr.jsonl.zst"),
        zstd::stream::encode_all(sealed.as_slice(), 0).unwrap(),
    )
    .unwrap();

    let output = fixture.run("denied", false);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(output.stdout, raw);
    assert!(fixture.aws_log().is_empty());

    fs::remove_file(fixture.session.join("herdr.jsonl")).unwrap();
    let output = fixture.run("denied", false);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(output.stdout, sealed);
    assert!(fixture.aws_log().is_empty());
}

#[test]
fn remote_sidecar_fetch_is_byte_identical_regular_and_then_local() {
    let fixture = Fixture::new();
    let body = b"{\"pane\":\"fetched\",\"siblings\":[]}\n";
    let compressed = fixture.write_object(body);

    let output = fixture.run("success", false);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(output.stdout, body);
    let destination = fixture.session.join("herdr.jsonl.zst");
    assert_eq!(fs::read(&destination).unwrap(), compressed);
    assert!(fs::symlink_metadata(&destination)
        .unwrap()
        .file_type()
        .is_file());
    let log = fixture.aws_log();
    assert!(log.contains("s3api head-object"), "{log}");
    assert!(log.contains("herdr.jsonl.zst"), "{log}");
    assert!(!log.contains("turns.jsonl.zst"), "{log}");

    fs::remove_file(&fixture.log).unwrap();
    let output = fixture.run("denied", false);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(output.stdout, body);
    assert!(fixture.aws_log().is_empty());
}

#[test]
fn disabled_missing_and_denied_fetches_fail_loudly() {
    let fixture = Fixture::new();
    fixture.write_object(b"unused\n");

    let output = fixture.run("denied", true);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("fetching is disabled"));
    assert!(fixture.aws_log().is_empty());

    let output = fixture.run("denied", false);
    let error = stderr(&output);
    assert!(!output.status.success());
    assert!(error.contains("AccessDenied"), "{error}");
    assert!(!error.contains("is in neither"), "{error}");

    fs::remove_file(&fixture.log).unwrap();
    let output = fixture.run("missing", false);
    let error = stderr(&output);
    assert!(!output.status.success());
    for date in ["2026/08/12", "2026/08/11", "2026/08/13"] {
        assert!(
            error.contains(&format!("sessions/{date}/{ID}/herdr.jsonl.zst")),
            "{error}"
        );
    }
    assert_eq!(fixture.aws_log().lines().count(), 3);
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
