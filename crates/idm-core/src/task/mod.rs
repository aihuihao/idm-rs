//! 下载编排、状态机与进度。

pub mod cancel;
pub mod download;
pub mod progress;
pub mod resume;
pub mod state;

pub use cancel::CancelSignal;
pub use download::{
    CheckpointHooks, DownloadContext, DownloadOptions, DownloadOutcome, PauseReason, ResumePlan,
    download, download_with_store,
};
pub use progress::{ChunkProgress, ProgressPublisher, ProgressSnapshot};
pub use resume::resume;
pub use state::{ChunkState, InvalidStateTransition, TaskState};
