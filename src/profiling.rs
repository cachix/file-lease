//! Opt-in metadata-only timings. Ordinary builds compile these guards away.

pub(crate) struct Span {
    #[cfg(feature = "unstable-profiling")]
    timing: Option<(&'static str, std::time::Instant)>,
}

impl Span {
    #[inline]
    pub(crate) fn new(_phase: &'static str) -> Self {
        Self {
            #[cfg(feature = "unstable-profiling")]
            timing: directory().map(|_| (_phase, std::time::Instant::now())),
        }
    }
}

#[cfg(feature = "unstable-profiling")]
fn directory() -> Option<&'static std::path::PathBuf> {
    static DIRECTORY: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    DIRECTORY
        .get_or_init(|| std::env::var_os("_FILE_LEASE_PROFILE_DIR").map(Into::into))
        .as_ref()
}

#[cfg(feature = "unstable-profiling")]
impl Drop for Span {
    fn drop(&mut self) {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};

        let Some((phase, start)) = self.timing else {
            return;
        };
        // Stop the clock before recording. Nested spans include diagnostic IO;
        // compare whole-lease timings against a build without this feature.
        let micros = start.elapsed().as_micros();
        let pid = std::process::id();
        let path = directory().unwrap().join(format!("profile-{pid}.csv"));
        // Record only a fixed phase name, PID, and elapsed time. Never record
        // payloads, filenames, command arguments, or journal contents.
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
        {
            let record = format!("{phase},{pid},{micros}\n");
            let _ = file.write_all(record.as_bytes());
        }
    }
}
