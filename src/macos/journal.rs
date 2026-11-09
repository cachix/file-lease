//! Durable attachment records and validation of their device and mount identity.
use super::filesystem::c_string;
use crate::{macos_registry as registry, profiling::Span};
use rustix::fs::fstatfs;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

pub(super) fn device_identity(metadata: &fs::Metadata) -> [u64; 4] {
    [
        metadata.ino(),
        metadata.rdev(),
        metadata.ctime() as u64,
        metadata.ctime_nsec() as u64,
    ]
}

#[derive(Serialize, Deserialize)]
pub(super) struct Journal {
    pub(super) keeper: u32,
    pub(super) birth: [u64; 2],
    #[serde(default)]
    pub(super) lease_id: Option<uuid::Uuid>,
    pub(super) device: String,
    pub(super) mount: PathBuf,
    pub(super) filesystem: Option<u64>,
    pub(super) device_identity: Option<[u64; 4]>,
    // None means an older journal with unknown attachment state. Only an
    // explicit false permits cleanup without identifying an attached device.
    #[serde(default)]
    pub(super) attachment_started: Option<bool>,
}

impl Journal {
    pub(super) fn identify_device(&mut self, directory: &Path) -> io::Result<()> {
        if self.device.is_empty() {
            let device = self
                .lease_id
                .map(registry::device)
                .transpose()?
                .flatten()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "RAM-disk attachment has not been identified yet",
                    )
                })?;
            self.device_identity = Some(device_identity(&fs::metadata(&device)?));
            self.device = device;
            self.save(directory)?;
        }
        Ok(())
    }

    pub(super) fn read(directory: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join("lease.json"))?;
        if file.metadata()?.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::other("File lease journal is not user-owned"));
        }
        serde_json::from_reader(file.take(16384)).map_err(io::Error::other)
    }

    pub(super) fn save(&self, directory: &Path) -> io::Result<()> {
        let _span = Span::new("journal.save");
        // Keep the previous complete record if a writer dies during an update.
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(&mut file, self).map_err(io::Error::other)?;
        {
            let _span = Span::new("journal.file_sync");
            file.as_file().sync_all()?;
        }
        file.persist(directory.join("lease.json"))
            .map_err(|error| error.error)?;
        let _span = Span::new("journal.directory_sync");
        fs::File::open(directory)?.sync_all()
    }

    pub(super) fn validate_mount(&self) -> io::Result<bool> {
        Ok(self.mounted_root()?.is_some())
    }

    pub(super) fn mounted_root(&self) -> io::Result<Option<(fs::File, rustix::fs::StatFs)>> {
        let _span = Span::new("mount.inspect");
        if !self.device.strip_prefix("/dev/disk").is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            return Err(io::Error::other(
                "Invalid RAM-disk device in file lease journal",
            ));
        }
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.mount)?;
        let info = fstatfs(&root)?;
        if c_string(&info.f_mntonname) != self.mount.as_os_str().as_bytes() {
            return Ok(None);
        }
        if c_string(&info.f_fstypename) != b"hfs"
            || c_string(&info.f_mntfromname) != self.device.as_bytes()
            || self.filesystem.is_some_and(|device| {
                root.metadata()
                    .map_or(true, |metadata| metadata.dev() != device)
            })
            || info.f_owner != unsafe { libc::geteuid() }
        {
            return Err(io::Error::other(
                "RAM-disk identity changed; refusing file lease cleanup",
            ));
        }
        Ok(Some((root, info)))
    }

    pub(super) fn validate_device(&self) -> io::Result<bool> {
        let _span = Span::new("device.validate");
        if self.device.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Cannot discard an unfinished RAM-disk attachment",
            ));
        }
        let metadata = match fs::metadata(&self.device) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if let Some(id) = self.lease_id {
            match registry::device(id)? {
                // Disk numbers are reused eagerly. With no device carrying our
                // identifier, this node belongs to another attachment and ours
                // is already gone.
                None => return Ok(false),
                Some(device) if device != self.device => {
                    return Err(io::Error::other("RAM-disk lease identifier changed"));
                }
                Some(_) => {}
            }
        }
        if Some(device_identity(&metadata)) != self.device_identity {
            return Err(io::Error::other("RAM-disk device identity changed"));
        }
        Ok(true)
    }
}
