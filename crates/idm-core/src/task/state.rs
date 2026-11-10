//! 任务与分块状态机（内存态，1c 再持久化）。所有状态转换经受控函数，非法转换报错。

use thiserror::Error;

/// 任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Pending,
    Queued,
    Probing,
    Downloading,
    Finalizing,
    Completed,
    Failed,
    Cancelled,
    Paused,
}

/// 分块状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChunkState {
    #[default]
    Pending,
    Downloading,
    Done,
    Failed,
    /// 协作式取消导致分块提前退出（不算失败，也不是完成）。
    Cancelled,
}

/// 非法状态转换错误（泛型，复用于任务与分块状态机）。
#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid state transition: {from:?} -> {to:?}")]
pub struct InvalidStateTransition<S: std::fmt::Debug + PartialEq + Eq> {
    pub from: S,
    pub to: S,
}

impl TaskState {
    /// 受控转换；非法时返回 [`InvalidStateTransition`]。
    pub fn transition(self, to: TaskState) -> Result<TaskState, InvalidStateTransition<TaskState>> {
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(InvalidStateTransition { from: self, to })
        }
    }

    /// 允许的转换集合。
    pub fn can_transition_to(self, to: TaskState) -> bool {
        use TaskState::*;
        matches!(
            (self, to),
            (Pending, Probing)
                | (Pending, Downloading)
                | (Probing, Downloading)
                | (Probing, Failed)
                | (Probing, Cancelled)
                | (Downloading, Finalizing)
                | (Downloading, Failed)
                | (Downloading, Cancelled)
                | (Downloading, Paused)
                | (Finalizing, Completed)
                | (Finalizing, Failed)
                | (Failed, Pending)
                | (Cancelled, Pending)
                | (Paused, Pending)
                | (Pending, Queued)
                | (Queued, Probing)
                | (Queued, Paused)
                | (Queued, Cancelled)
                | (Paused, Queued)
                | (Failed, Queued)
        )
    }
}

impl ChunkState {
    /// 受控转换；非法时返回 [`InvalidStateTransition`]。
    pub fn transition(
        self,
        to: ChunkState,
    ) -> Result<ChunkState, InvalidStateTransition<ChunkState>> {
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(InvalidStateTransition { from: self, to })
        }
    }

    pub fn can_transition_to(self, to: ChunkState) -> bool {
        use ChunkState::*;
        matches!(
            (self, to),
            (Pending, Downloading)
                | (Downloading, Done)
                | (Downloading, Failed)
                | (Downloading, Cancelled)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_legal_transitions() {
        let mut s = TaskState::Pending;
        s = s.transition(TaskState::Probing).unwrap();
        s = s.transition(TaskState::Downloading).unwrap();
        s = s.transition(TaskState::Finalizing).unwrap();
        s = s.transition(TaskState::Completed).unwrap();
        assert_eq!(s, TaskState::Completed);
    }

    #[test]
    fn task_illegal_transition() {
        let r = TaskState::Pending.transition(TaskState::Completed);
        assert_eq!(
            r,
            Err(InvalidStateTransition {
                from: TaskState::Pending,
                to: TaskState::Completed
            })
        );
        // Downloading 不能直接到 Completed。
        assert!(
            TaskState::Downloading
                .transition(TaskState::Completed)
                .is_err()
        );
        // Completed 不可再转。
        assert!(TaskState::Completed.transition(TaskState::Pending).is_err());
    }

    #[test]
    fn chunk_legal_and_illegal() {
        let mut c = ChunkState::Pending;
        c = c.transition(ChunkState::Downloading).unwrap();
        c = c.transition(ChunkState::Done).unwrap();
        assert_eq!(c, ChunkState::Done);
        assert_eq!(
            ChunkState::Pending.transition(ChunkState::Done),
            Err(InvalidStateTransition {
                from: ChunkState::Pending,
                to: ChunkState::Done
            })
        );
        assert!(ChunkState::Done.transition(ChunkState::Failed).is_err());
    }

    #[test]
    fn chunk_cancelled_is_a_terminal_state() {
        let s = ChunkState::Downloading
            .transition(ChunkState::Cancelled)
            .unwrap();
        assert_eq!(s, ChunkState::Cancelled);
        assert!(
            ChunkState::Pending
                .transition(ChunkState::Cancelled)
                .is_err()
        );
        assert!(ChunkState::Done.transition(ChunkState::Cancelled).is_err());
        assert!(ChunkState::Cancelled.transition(ChunkState::Done).is_err());
    }
}
