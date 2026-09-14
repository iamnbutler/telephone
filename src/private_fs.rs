//! Private state files. Same-UID processes remain inside the trust boundary.
use anyhow::{bail, Context, Result};
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

fn check_owner(file: &File) -> Result<()> {
    // SAFETY: geteuid takes no arguments and does not access caller memory.
    let uid = unsafe { libc::geteuid() };
    if file.metadata()?.uid() != uid {
        bail!("state path is owned by a different user");
    }
    Ok(())
}

pub fn directory(path: &Path) -> Result<File> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating private directory {path:?}")),
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("opening private directory {path:?}"))?;
    check_owner(&file)?;
    file.set_permissions(Permissions::from_mode(0o700))
        .context("restricting state directory permissions")?;
    Ok(file)
}

/// Validate without opening a descriptor: closing any independently opened
/// database or SHM descriptor releases SQLite's process-wide POSIX locks.
/// Only use beneath our already-validated owner-only state directory.
pub fn check_state_file(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("checking state file {path:?}")),
    };
    // SAFETY: geteuid has no arguments or caller-owned memory.
    if metadata.uid() != unsafe { libc::geteuid() } {
        bail!("state path is owned by a different user");
    }
    if !metadata.is_file() || metadata.nlink() != 1 {
        bail!("state path must be a regular file with one link");
    }
    match fs::set_permissions(path, Permissions::from_mode(0o600)) {
        Ok(()) => Ok(()),
        // SQLite may remove a sidecar between stat and chmod.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("restricting state file permissions"),
    }
}

pub fn read_owned(path: &Path, limit: usize) -> Result<String> {
    read_record(path, limit, false)
}
pub fn read_secret(path: &Path, limit: usize) -> Result<String> {
    read_record(path, limit, true)
}
fn read_record(path: &Path, limit: usize, private: bool) -> Result<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("opening record {path:?}"))?;
    check_owner(&file)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > limit as u64 {
        bail!("record must be a bounded regular file with one link");
    }
    if private && metadata.permissions().mode() & 0o077 != 0 {
        bail!("secret file is accessible to other users");
    }
    let mut text = String::new();
    file.take(limit as u64 + 1)
        .read_to_string(&mut text)
        .context("reading bounded record")?;
    if text.len() > limit {
        bail!("record exceeds size limit");
    }
    Ok(text)
}

/// Reject sidecar symlinks before SQLite opens them. The enclosing directory is
/// owner-only; defending against a malicious process with our UID needs isolation.
pub fn check_sidecars(db: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = db.as_os_str().to_os_string();
        name.push(suffix);
        check_state_file(Path::new(&name))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_only_and_no_symlinks_or_hardlinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        directory(&root).unwrap();
        let path = root.join("data");
        fs::write(&path, b"fixture").unwrap();
        check_state_file(&path).unwrap();
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::os::unix::fs::symlink(&path, root.join("link")).unwrap();
        assert!(check_state_file(&root.join("link")).is_err());
        fs::hard_link(&path, root.join("hard")).unwrap();
        assert!(check_state_file(&path).is_err());
    }
}
