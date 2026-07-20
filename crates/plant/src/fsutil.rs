use std::io::Write;
use std::path::Path;

/// Atomically replace `path` from a unique temporary file in the same directory.
pub fn atomic_replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let result = std::fs::File::create(&tmp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|_| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}
