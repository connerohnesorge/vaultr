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

/// The default holds several times the measured 18 MiB peak demand of one
/// capture replacement write, leaving room for the small `.meta` drop marker.
pub fn headroom_floor() -> u64 {
    std::env::var("PLANT_CAPTURE_HEADROOM_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64 * 1024 * 1024)
}
