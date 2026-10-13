//! Shared state for one copy/move/extract operation: progress, conflicts,
//! cancellation. Used by `oc-vfs` (cross-file-system copies) and by
//! `oc-local` (native local copies), so the UI drives both the same way.

use crate::{Result, VfsError};

/// The user's answer when a destination already exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    Overwrite,
    Skip,
    Cancel,
}

/// Snapshot handed to the progress callback. `current` is a display string.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    pub current: String,
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// Callbacks for a transfer. They are called from the worker thread doing
/// the transfer; `on_conflict` may block while the UI asks the user.
pub struct TransferCallbacks<'a> {
    /// Called with (source display, destination display) when the destination exists.
    pub on_conflict: &'a mut dyn FnMut(&str, &str) -> Conflict,
    pub on_progress: Option<&'a mut dyn FnMut(&Progress)>,
    pub is_cancelled: &'a dyn Fn() -> bool,
}

/// Running state of one operation.
pub struct Transfer<'a> {
    pub callbacks: TransferCallbacks<'a>,
    pub progress: Progress,
}

impl<'a> Transfer<'a> {
    pub fn new(callbacks: TransferCallbacks<'a>, files_total: u64, bytes_total: u64) -> Self {
        Transfer { callbacks, progress: Progress { files_total, bytes_total, ..Progress::default() } }
    }

    pub fn check_cancel(&self) -> Result<()> {
        if (self.callbacks.is_cancelled)() { Err(VfsError::Cancelled) } else { Ok(()) }
    }

    pub fn report(&mut self, current: Option<&str>) {
        if let Some(c) = current {
            self.progress.current = c.to_string();
        }
        if let Some(cb) = self.callbacks.on_progress.as_mut() {
            cb(&self.progress);
        }
    }

    /// Ask about an existing destination. Ok(true) = write it, Ok(false) = skip,
    /// Err(Cancelled) = stop everything. The caller removes the old item.
    pub fn ask_conflict(&mut self, src: &str, dst: &str) -> Result<bool> {
        match (self.callbacks.on_conflict)(src, dst) {
            Conflict::Overwrite => Ok(true),
            Conflict::Skip => Ok(false),
            Conflict::Cancel => Err(VfsError::Cancelled),
        }
    }
}
