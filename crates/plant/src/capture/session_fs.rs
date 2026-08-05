//! Neutral retained-descriptor filesystem primitives shared by capture
//! persistence and immutable-generation maintenance.

use std::ffi::CString;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// One retained, no-follow session directory. Cooperative maintenance callers
/// hold its flock through temp recovery, commit, and cleanup.
pub(super) struct SessionDirectory {
    path: PathBuf,
    file: File,
}

impl Drop for SessionDirectory {
    fn drop(&mut self) {
        // A forked subprocess can briefly retain this open-file description
        // until exec applies O_CLOEXEC. Unlock explicitly so that inherited
        // duplicate does not extend a completed maintenance transaction.
        // A failed unlock cannot be reported from Drop.
        // SAFETY: the retained directory descriptor is valid until File drops.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl SessionDirectory {
    pub(super) fn open(path: &Path) -> Result<Self, String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|error| format!("open session directory {}: {error}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    fn name(&self, name: &str) -> Result<CString, String> {
        if name.is_empty()
            || Path::new(name).file_name().and_then(|part| part.to_str()) != Some(name)
        {
            return Err(format!(
                "invalid session entry name under {}",
                self.path.display()
            ));
        }
        CString::new(name)
            .map_err(|_| format!("invalid session entry name under {}", self.path.display()))
    }

    fn open_with_flags(&self, name: &str, flags: i32, mode: u32) -> Result<Option<File>, String> {
        let name_c = self.name(name)?;
        // SAFETY: `name_c` is NUL-terminated, the retained directory fd is
        // valid, and a successful descriptor is transferred into `File`.
        let descriptor =
            unsafe { libc::openat(self.file.as_raw_fd(), name_c.as_ptr(), flags, mode) };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(format!(
                "open session entry {}: {error}",
                self.path.join(name).display()
            ));
        }
        // SAFETY: `openat` returned a new owned descriptor.
        let file = unsafe { File::from_raw_fd(descriptor) };
        if !file
            .metadata()
            .map_err(|error| {
                format!(
                    "inspect session entry {}: {error}",
                    self.path.join(name).display()
                )
            })?
            .is_file()
        {
            return Err(format!(
                "session entry is not a regular file at {}",
                self.path.join(name).display()
            ));
        }
        Ok(Some(file))
    }

    pub(super) fn open_optional(&self, name: &str, write: bool) -> Result<Option<File>, String> {
        let access = if write { libc::O_RDWR } else { libc::O_RDONLY };
        self.open_with_flags(
            name,
            access | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0,
        )
    }

    pub(super) fn open_required(&self, name: &str, write: bool) -> Result<File, String> {
        self.open_optional(name, write)?.ok_or_else(|| {
            format!(
                "missing session entry at {}",
                self.path.join(name).display()
            )
        })
    }

    pub(super) fn open_append(&self, name: &str, create: bool) -> Result<Option<File>, String> {
        let mut flags =
            libc::O_RDWR | libc::O_APPEND | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        if create {
            flags |= libc::O_CREAT;
        }
        self.open_with_flags(name, flags, 0o666)
    }

    pub(super) fn create_temp(&self, base: &str, purpose: &str) -> Result<(String, File), String> {
        let name = format!(".{base}.{purpose}-{}", uuid::Uuid::new_v4());
        let name_c = self.name(&name)?;
        let flags =
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        // SAFETY: `name_c` is NUL-terminated, the retained directory fd is
        // valid, and a successful descriptor is transferred into `File`.
        let descriptor =
            unsafe { libc::openat(self.file.as_raw_fd(), name_c.as_ptr(), flags, 0o600) };
        if descriptor < 0 {
            return Err(format!(
                "create session temp {}: {}",
                self.path.join(&name).display(),
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: `openat` returned a new owned descriptor.
        Ok((name, unsafe { File::from_raw_fd(descriptor) }))
    }

    pub(super) fn entry_names(&self) -> Result<Vec<String>, String> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.path)
            .map_err(|error| format!("read session directory {}: {error}", self.path.display()))?
        {
            let entry = entry.map_err(|error| {
                format!("read session entry under {}: {error}", self.path.display())
            })?;
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
        let current = Self::open(&self.path)?;
        if !Self::same_file(&self.file, &current.file)? {
            return Err(format!(
                "session directory changed during inventory at {}",
                self.path.display()
            ));
        }
        Ok(names)
    }

    pub(super) fn cleanup_temps(
        &self,
        base: &str,
        purposes: &[&str],
        legacy_names: &[&str],
        forbidden_names: &[&str],
    ) -> Result<(), String> {
        let mut owned_names = Vec::new();
        for name in self.entry_names()? {
            let mut prefixed = false;
            let exact_uuid = purposes.iter().any(|purpose| {
                let prefix = format!(".{base}.{purpose}-");
                name.strip_prefix(&prefix).is_some_and(|suffix| {
                    prefixed = true;
                    uuid::Uuid::parse_str(suffix).is_ok_and(|id| {
                        id.get_version_num() == 4 && id.hyphenated().to_string() == suffix
                    })
                })
            });
            let exact_legacy = legacy_names.iter().any(|legacy| name == *legacy);
            let legacy_near_miss = legacy_names.iter().any(|legacy| {
                legacy
                    .strip_suffix("tmp")
                    .is_some_and(|prefix| name != *legacy && name.starts_with(prefix))
            });
            let forbidden = forbidden_names.iter().any(|forbidden| {
                name == *forbidden
                    || forbidden
                        .strip_suffix("tmp")
                        .is_some_and(|prefix| name.starts_with(prefix))
            });
            if (prefixed && !exact_uuid) || legacy_near_miss || forbidden {
                return Err(format!(
                    "unrecognized session temp evidence at {}",
                    self.path.join(name).display()
                ));
            }
            if exact_uuid || exact_legacy {
                owned_names.push(name);
            }
        }

        // Retain every exact regular entry before removing any. A symlink or
        // non-regular legacy entry leaves all evidence untouched.
        let mut owned = Vec::with_capacity(owned_names.len());
        for name in owned_names {
            let file = self.open_required(&name, false)?;
            owned.push((name, file));
        }
        for (name, file) in owned {
            self.unlink_if_same(&name, &file)?;
        }
        Ok(())
    }

    fn same_file(left: &File, right: &File) -> Result<bool, String> {
        let left = left
            .metadata()
            .map_err(|error| format!("inspect retained session entry: {error}"))?;
        let right = right
            .metadata()
            .map_err(|error| format!("inspect current session entry: {error}"))?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }

    pub(super) fn entry_matches(&self, name: &str, expected: &File) -> Result<bool, String> {
        let Some(current) = self.open_optional(name, false)? else {
            return Ok(false);
        };
        Self::same_file(&current, expected)
    }

    pub(super) fn sync(&self) -> Result<(), String> {
        self.file
            .sync_all()
            .map_err(|error| format!("sync session directory {}: {error}", self.path.display()))
    }

    pub(super) fn lock_exclusive(&self) -> Result<(), String> {
        // SAFETY: the retained directory descriptor remains valid for this
        // object's lifetime; `flock` does not take ownership of it.
        if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "lock session directory {}: {}",
                self.path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    pub(super) fn replace_entry(
        &self,
        from: &str,
        to: &str,
        source: &File,
        expected_destination: Option<&File>,
    ) -> Result<File, String> {
        source.sync_all().map_err(|error| {
            format!(
                "sync session entry before rename {}: {error}",
                self.path.join(from).display()
            )
        })?;
        if !self.entry_matches(from, source)? {
            return Err(format!(
                "session entry changed before rename at {}",
                self.path.join(from).display()
            ));
        }
        match (self.open_optional(to, false)?, expected_destination) {
            (Some(current), Some(expected)) if Self::same_file(&current, expected)? => {}
            (None, None) => {}
            _ => {
                return Err(format!(
                    "session destination changed before rename at {}",
                    self.path.join(to).display()
                ));
            }
        }
        let from_c = self.name(from)?;
        let to_c = self.name(to)?;
        // SAFETY: both names are NUL-terminated and resolved relative to the
        // retained directory descriptor.
        let status = unsafe {
            libc::renameat(
                self.file.as_raw_fd(),
                from_c.as_ptr(),
                self.file.as_raw_fd(),
                to_c.as_ptr(),
            )
        };
        if status != 0 {
            return Err(format!(
                "rename session entry {}: {}",
                self.path.join(from).display(),
                std::io::Error::last_os_error()
            ));
        }
        let renamed = self.open_required(to, false)?;
        if !Self::same_file(&renamed, source)? {
            return Err(format!(
                "session entry changed during rename at {}",
                self.path.join(to).display()
            ));
        }
        self.sync()?;
        source
            .try_clone()
            .map_err(|error| format!("retain renamed session entry: {error}"))
    }

    pub(super) fn unlink_if_same(&self, name: &str, expected: &File) -> Result<(), String> {
        if !self.entry_matches(name, expected)? {
            return Err(format!(
                "session entry changed before removal at {}",
                self.path.join(name).display()
            ));
        }
        let name_c = self.name(name)?;
        // SAFETY: `name_c` is NUL-terminated and resolved relative to the
        // retained directory descriptor.
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), name_c.as_ptr(), 0) } != 0 {
            return Err(format!(
                "remove session entry {}: {}",
                self.path.join(name).display(),
                std::io::Error::last_os_error()
            ));
        }
        self.sync()
    }

    #[cfg(test)]
    pub(super) fn try_lock_exclusive(&self) -> Result<(), std::io::Error> {
        // SAFETY: the retained directory descriptor remains valid for this call.
        let status = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if status == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

pub(super) fn clone_at_start(file: &File) -> Result<File, String> {
    let mut clone = file
        .try_clone()
        .map_err(|error| format!("clone retained session entry: {error}"))?;
    clone
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek retained session entry: {error}"))?;
    Ok(clone)
}

pub(super) fn hash_file(file: &File) -> Result<String, String> {
    vaultr::vault::sha256_reader(clone_at_start(file)?).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn preoperation_entry_substitutions_are_rejected() {
        use std::os::unix::fs::symlink;

        let root =
            std::env::temp_dir().join(format!("plant-anchored-swap-{}", uuid::Uuid::new_v4()));
        let session = root.join("session");
        std::fs::create_dir_all(&session).unwrap();
        let outside = root.join("outside");
        std::fs::write(&outside, b"outside evidence\n").unwrap();
        let outside_before = std::fs::read(&outside).unwrap();
        let directory = SessionDirectory::open(&session).unwrap();

        std::fs::write(session.join("turns.jsonl"), b"capture evidence\n").unwrap();
        let raw = directory.open_required("turns.jsonl", true).unwrap();
        std::fs::rename(session.join("turns.jsonl"), session.join("retained-turns")).unwrap();
        symlink(&outside, session.join("turns.jsonl")).unwrap();
        assert!(directory
            .replace_entry("turns.jsonl", "turns.jsonl.sealing-0-deadbeef", &raw, None,)
            .is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), outside_before);

        let detached = "herdr.jsonl.sealing-0-deadbeef";
        std::fs::write(session.join(detached), b"herdr evidence\n").unwrap();
        let retained = directory.open_required(detached, false).unwrap();
        std::fs::rename(session.join(detached), session.join("retained-herdr")).unwrap();
        symlink(&outside, session.join(detached)).unwrap();
        assert!(directory.unlink_if_same(detached, &retained).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), outside_before);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn independent_directory_owners_are_serialized() {
        let root =
            std::env::temp_dir().join(format!("plant-session-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let first = SessionDirectory::open(&root).unwrap();
        let second = SessionDirectory::open(&root).unwrap();
        first.lock_exclusive().unwrap();
        assert_eq!(
            second.try_lock_exclusive().unwrap_err().raw_os_error(),
            Some(libc::EAGAIN)
        );
        drop(first);
        second.try_lock_exclusive().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn drop_unlocks_before_an_inherited_duplicate_closes() {
        let root =
            std::env::temp_dir().join(format!("plant-session-unlock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let first = SessionDirectory::open(&root).unwrap();
        first.lock_exclusive().unwrap();
        let inherited = first.file.try_clone().unwrap();

        drop(first);

        let second = SessionDirectory::open(&root).unwrap();
        second
            .try_lock_exclusive()
            .expect("explicit Drop unlock must not wait for a duplicate fd");
        drop(inherited);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_temp_cleanup_is_all_or_nothing_for_unsafe_entries() {
        use std::os::unix::fs::symlink;

        let legacy = &["turns.jsonl.frame-tmp", "turns.jsonl.zst-tmp"];
        for case in ["symlink", "directory", "near-miss"] {
            let root = std::env::temp_dir()
                .join(format!("plant-legacy-temp-{case}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let retained = root.join("turns.jsonl.zst-tmp");
            std::fs::write(&retained, b"exact regular evidence").unwrap();
            let outside = root.with_extension(format!("{case}-outside"));
            std::fs::write(&outside, b"outside evidence").unwrap();
            let outside_before = std::fs::read(&outside).unwrap();
            let suspect = match case {
                "symlink" => {
                    let path = root.join("turns.jsonl.frame-tmp");
                    symlink(&outside, &path).unwrap();
                    path
                }
                "directory" => {
                    let path = root.join("turns.jsonl.frame-tmp");
                    std::fs::create_dir(&path).unwrap();
                    path
                }
                "near-miss" => {
                    let path = root.join("turns.jsonl.frame-tm");
                    std::fs::write(&path, b"near-miss evidence").unwrap();
                    path
                }
                _ => unreachable!(),
            };
            let directory = SessionDirectory::open(&root).unwrap();
            directory.lock_exclusive().unwrap();
            assert!(directory
                .cleanup_temps("turns.jsonl.zst", &["frame", "merged"], legacy, &[])
                .is_err());
            assert!(std::fs::symlink_metadata(&suspect).is_ok());
            assert_eq!(std::fs::read(&retained).unwrap(), b"exact regular evidence");
            assert_eq!(std::fs::read(&outside).unwrap(), outside_before);
            drop(directory);
            let _ = std::fs::remove_dir_all(root);
            let _ = std::fs::remove_file(outside);
        }
    }
}
