//! Atomic 0600 JSONL writes shared by the native session writers.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::Path;

/// Write `lines` to `dest` atomically: refuse to overwrite an existing file,
/// stage into a same-directory temp file with mode 0600, then rename into
/// place. No partial file is ever left at `dest` on any failure path.
pub fn write_atomic_0600(dest: &Path, lines: &[String]) -> Result<()> {
    if dest.exists() {
        bail!(
            "refusing to overwrite existing session file {}",
            dest.display()
        );
    }
    let dir = dest
        .parent()
        .with_context(|| format!("no parent directory for {}", dest.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        dest.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("create temp file {}", tmp.display()))?;
        for line in lines {
            f.write_all(line.as_bytes())?;
            f.write_all(b"\n")?;
        }
        f.sync_all()?;
        // Re-check just before rename: never clobber a file that appeared.
        if dest.exists() {
            bail!(
                "refusing to overwrite existing session file {}",
                dest.display()
            );
        }
        std::fs::rename(&tmp, dest).with_context(|| format!("rename into {}", dest.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Truncate a string to at most `max` bytes on a char boundary.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}
