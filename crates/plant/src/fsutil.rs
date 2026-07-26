use std::io::Write;
use std::path::Path;

/// Free bytes available to this process on the volume holding `path`.
/// `None` when the volume cannot be measured — callers must not read that as full.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

/// Free space below this leaves the small `.meta` drop marker unwritable, so
/// capture skips the multi-megabyte journal write instead.
pub fn headroom_floor() -> u64 {
    std::env::var("PLANT_CAPTURE_HEADROOM_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024 * 1024)
}

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
