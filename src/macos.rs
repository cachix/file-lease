//! macOS RAM-volume storage and process-owned cleanup.
mod filesystem;
mod journal;
mod process;
mod recovery;
mod setup;
mod storage;
mod tools;

pub(super) use process::{process_info, watch_owner};
pub(super) use setup::maybe_run_setup;
pub(super) use storage::Storage;

#[cfg(test)]
mod tests;
