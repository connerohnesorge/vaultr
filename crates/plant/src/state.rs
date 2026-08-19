use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub(crate) fn dir() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = TEST_DIR.with(|slot| slot.borrow().clone()) {
        return path;
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state/plant")
}

#[cfg(test)]
thread_local! {
    static TEST_DIR: std::cell::RefCell<Option<PathBuf>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
pub(crate) struct TestDirGuard;

#[cfg(test)]
impl Drop for TestDirGuard {
    fn drop(&mut self) {
        TEST_DIR.with(|slot| {
            slot.replace(None);
        });
    }
}

#[cfg(test)]
pub(crate) fn use_test_dir(path: PathBuf) -> TestDirGuard {
    TEST_DIR.with(|slot| {
        assert!(
            slot.replace(Some(path)).is_none(),
            "test state already scoped"
        );
    });
    TestDirGuard
}

pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

pub(crate) fn ensure_dir_durable(path: &Path) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut cursor = path;
    loop {
        match std::fs::metadata(cursor) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} is not a directory", cursor.display()),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(cursor.to_path_buf());
                cursor = cursor.parent().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "directory has no existing parent",
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }
    for dir in missing.into_iter().rev() {
        match std::fs::create_dir(&dir) {
            Ok(()) => {}
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists
                    && std::fs::metadata(&dir)?.is_dir() => {}
            Err(error) => return Err(error),
        }
        sync_dir(&dir)?;
        sync_dir(dir.parent().expect("created directory has a parent"))?;
    }
    Ok(())
}

/// Which crash guarantee a replacement write buys. The caller states it; the
/// guarantee is never implied by which helper happened to be in scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Durability {
    /// Atomic for a reader, but nothing is fsynced: after a power loss the
    /// replacement may be absent or truncated. Only for state a later pass can
    /// rebuild, or where the fsync barrier costs more than the data is worth.
    Rename,
    /// The bytes and the directory entry are both on stable storage before the
    /// call returns. Required for anything a recovery pointer refers to.
    Fsync,
}

/// Atomically replace `path` from a unique temporary file in the same
/// directory, with the crash guarantee named by `durability`.
pub(crate) fn replace_file(path: &Path, bytes: &[u8], durability: Durability) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    match durability {
        Durability::Rename => std::fs::create_dir_all(parent)?,
        Durability::Fsync => ensure_dir_durable(parent)?,
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = match durability {
            Durability::Rename => File::create(&tmp)?,
            Durability::Fsync => OpenOptions::new().write(true).create_new(true).open(&tmp)?,
        };
        file.write_all(bytes)?;
        if durability == Durability::Fsync {
            file.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        if durability == Durability::Fsync {
            sync_dir(parent)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    #[cfg(test)]
    if result.is_ok() {
        record_write(path, durability);
    }
    result
}

#[cfg(test)]
static WRITE_LOG: std::sync::Mutex<Vec<(PathBuf, Durability)>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn record_write(path: &Path, durability: Durability) {
    let mut log = WRITE_LOG.lock().unwrap_or_else(|error| error.into_inner());
    log.push((path.to_path_buf(), durability));
}

/// The durability of the most recent successful [`replace_file`] of `path`.
/// Lets a test pin a call site's guarantee so a later edit cannot quietly
/// downgrade it.
#[cfg(test)]
pub(crate) fn recorded_durability(path: &Path) -> Option<Durability> {
    let log = WRITE_LOG.lock().unwrap_or_else(|error| error.into_inner());
    log.iter()
        .rev()
        .find(|(recorded, _)| recorded == path)
        .map(|(_, durability)| *durability)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_directory_creation_handles_nested_and_existing_paths() {
        let root = std::env::temp_dir().join(format!(
            "plant-state-dirs-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&root).unwrap();
        let nested = root.join("a").join("b");
        ensure_dir_durable(&nested).unwrap();
        ensure_dir_durable(&nested).unwrap();
        assert!(nested.is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }
}
