use std::path::PathBuf;
use crate::{Cancellation, Completion, Context, EventSink, Jobs};

#[derive(Clone, Debug)]
pub enum SyncOperands {
    Exact { source: PathBuf, destination: PathBuf },
    Into { directory: PathBuf, sources: Vec<PathBuf> },
    Contents { source: PathBuf, directory: PathBuf },
}

#[derive(Clone, Debug, Default)]
pub struct SyncOptions {
    pub delete: bool,
    pub dry_run: bool,
    pub jobs: Option<Jobs>,
}

#[derive(Clone, Debug)]
pub struct SyncRequest {
    pub operands: SyncOperands,
    pub options: SyncOptions,
    pub context: Context,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SyncProgress {
    pub examined_files: u64,
    pub transferred_files: u64,
    pub skipped_files: u64,
    pub deleted_files: u64,
    pub bytes_transferred: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncOutcome {
    pub progress: SyncProgress,
    pub completion: Completion,
}

impl SyncOutcome {
    pub const fn status(&self) -> i32 {
        match self.completion {
            Completion::Complete => 0,
            Completion::Failed => 1,
            Completion::InvalidRequest => 2,
            Completion::Cancelled => 130,
        }
    }
}

pub fn execute(
    request: &SyncRequest,
    cancellation: &Cancellation,
    sink: &dyn EventSink,
) -> SyncOutcome {
    let _ = (request, cancellation, sink);
    SyncOutcome {
        progress: SyncProgress::default(),
        completion: Completion::Complete,
    }
}
