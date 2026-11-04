//! 下载编排、状态机与进度。

pub mod download;
pub mod progress;
pub mod state;

pub use download::{DownloadOptions, DownloadOutcome, PauseReason, download};
pub use progress::{ChunkProgress, ProgressPublisher, ProgressSnapshot};
pub use state::{ChunkState, InvalidStateTransition, TaskState};
