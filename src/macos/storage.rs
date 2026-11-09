//! Payload allocation and revocation through a pinned, validated RAM mount.
use super::{filesystem::unlink_payload, recovery::cleanup_with_retry, setup::Volume};
use crate::profiling::Span;
#[cfg(feature = "shared-volumes")]
use rustix::fs::fstatfs;
use rustix::fs::{AtFlags, Mode, OFlags, openat, unlinkat};
use std::{
    fs,
    io::{self, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

#[derive(Default)]
pub(crate) struct Storage {
    pub(super) volume: Option<Volume>,
    pub(super) counter: u64,
    pub(super) files: std::collections::HashSet<PathBuf>,
}
impl Storage {
    pub(crate) fn materialize(&mut self, bytes: &[u8], suffix: &str) -> io::Result<PathBuf> {
        let _span = Span::new("storage.allocate");
        if self.volume.is_none() {
            self.volume = Some(Volume::create()?);
        }
        let volume = self.volume.as_ref().unwrap();
        let root = volume.open_root()?;
        #[cfg(feature = "shared-volumes")]
        {
            // Query the pinned, validated mount before opening a payload file.
            // This rejects known-full volumes without writing partial payloads.
            // Free space is only a preflight: metadata or other writers may
            // still cause ENOSPC during the write, which remains authoritative.
            let info = fstatfs(&root)?;
            if bytes.len() as u64 > info.f_bavail.saturating_mul(u64::from(info.f_bsize)) {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
        }
        let _span = Span::new("file.write");
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Secret file namespace exhausted"))?;
        let name = format!("secret-{}-{suffix}", self.counter);
        let path = volume.journal.mount.join(&name);
        let fd = {
            let _span = Span::new("file.open");
            openat(
                &root,
                &name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )?
        };
        let mut file = fs::File::from(fd);
        let write = {
            let _span = Span::new("file.write_bytes");
            file.write_all(bytes)
        };
        if let Err(error) = write.and_then(|_| {
            let _span = Span::new("file.permissions");
            file.set_permissions(fs::Permissions::from_mode(0o400))
        }) {
            let _ = unlinkat(&root, &name, AtFlags::empty());
            return Err(error);
        }
        self.files.insert(path.clone());
        {
            let _span = Span::new("file.close");
            drop(file);
        }
        Ok(path)
    }
    pub(crate) fn release(&mut self, path: &Path) -> io::Result<()> {
        if !self.files.contains(path) {
            return Err(io::Error::other("Path does not belong to this lease"));
        }
        let volume = self
            .volume
            .as_ref()
            .ok_or_else(|| io::Error::other("Secret storage is closed"))?;
        let root = volume.open_root()?;
        let name = path
            .strip_prefix(&volume.journal.mount)
            .ok()
            .filter(|name| name.components().count() == 1)
            .ok_or_else(|| io::Error::other("Invalid secret file path"))?;
        // Consumers own the volume's user and may delete a payload after
        // reading it. The validated root rules out a missing mount, so a
        // missing name is already revoked.
        unlink_payload(&root, name)?;
        self.files.remove(path);
        Ok(())
    }

    #[cfg(feature = "shared-volumes")]
    pub(crate) fn contains(&self, path: &Path) -> bool {
        self.files.contains(path)
    }

    #[cfg(feature = "shared-volumes")]
    pub(crate) fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub(crate) fn cleanup(&mut self) -> io::Result<()> {
        if let Some(volume) = self.volume.take() {
            let _span = Span::new("storage.cleanup");
            // Failed cleanup stays journaled. The guardian retries after this
            // keeper exits, and later leases remain a fallback if both die.
            cleanup_with_retry(&volume.directory)?;
        }
        self.files.clear();
        Ok(())
    }
}
impl Drop for Storage {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}
