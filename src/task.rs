//! Lightweight snipe task lifecycle + single-flight guard (no DB).
//! WAITING → ARMED → FIRING → SUCCESS | FAILED | CANCELLED

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TaskState {
    Idle = 0,
    Waiting = 1,
    Armed = 2,
    Firing = 3,
    Success = 4,
    Failed = 5,
    Cancelled = 6,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Waiting => "WAITING",
            Self::Armed => "ARMED",
            Self::Firing => "FIRING",
            Self::Success => "SUCCESS",
            Self::Failed => "FAILED",
            Self::Cancelled => "CANCELLED",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Idle | Self::Success | Self::Failed | Self::Cancelled
        )
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Waiting | Self::Armed | Self::Firing)
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Waiting,
            2 => Self::Armed,
            3 => Self::Firing,
            4 => Self::Success,
            5 => Self::Failed,
            6 => Self::Cancelled,
            _ => Self::Idle,
        }
    }
}

/// Process-wide single-flight + cancel flag for the live snipe.
#[derive(Clone, Default)]
pub struct TaskGate {
    inner: Arc<TaskGateInner>,
}

struct TaskGateInner {
    state: AtomicU8,
    cancel: AtomicBool,
    /// Prevents double-fire even if two paths race past state checks.
    fired: AtomicBool,
}

impl Default for TaskGateInner {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(TaskState::Idle as u8),
            cancel: AtomicBool::new(false),
            fired: AtomicBool::new(false),
        }
    }
}

impl TaskGate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> TaskState {
        TaskState::from_u8(self.inner.state.load(Ordering::Acquire))
    }

    pub fn is_active(&self) -> bool {
        self.state().is_active()
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancel.load(Ordering::Acquire)
    }

    /// Begin WAITING. Fails if another task is already active.
    pub fn begin_waiting(&self) -> eyre::Result<()> {
        // CAS so two concurrent begins cannot both pass the active check.
        loop {
            let cur_u8 = self.inner.state.load(Ordering::Acquire);
            let cur = TaskState::from_u8(cur_u8);
            if cur.is_active() {
                eyre::bail!(
                    "duplicate execution blocked — task already {} (cancel first)",
                    cur.as_str()
                );
            }
            self.inner.cancel.store(false, Ordering::Release);
            self.inner.fired.store(false, Ordering::Release);
            match self.inner.state.compare_exchange(
                cur_u8,
                TaskState::Waiting as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    crate::outln!("task → WAITING");
                    return Ok(());
                }
                Err(_) => continue,
            }
        }
    }

    pub fn set_armed(&self) {
        if self.is_cancelled() {
            return;
        }
        self.inner
            .state
            .store(TaskState::Armed as u8, Ordering::Release);
        crate::outln!("task → ARMED");
    }

    /// Transition to FIRING. Returns false if already fired or cancelled (no double-fire).
    pub fn try_begin_firing(&self) -> bool {
        if self.is_cancelled() {
            return false;
        }
        if self
            .inner
            .fired
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            crate::outln!("task double-fire blocked");
            return false;
        }
        self.inner
            .state
            .store(TaskState::Firing as u8, Ordering::Release);
        crate::outln!("task → FIRING");
        true
    }

    pub fn finish_success(&self) {
        self.inner
            .state
            .store(TaskState::Success as u8, Ordering::Release);
        crate::outln!("task → SUCCESS");
    }

    pub fn finish_failed(&self) {
        self.inner
            .state
            .store(TaskState::Failed as u8, Ordering::Release);
        crate::outln!("task → FAILED");
    }

    pub fn request_cancel(&self) {
        self.inner.cancel.store(true, Ordering::Release);
        self.inner
            .state
            .store(TaskState::Cancelled as u8, Ordering::Release);
        crate::outln!("task → CANCELLED");
    }

    pub fn reset_idle(&self) {
        self.inner.cancel.store(false, Ordering::Release);
        self.inner
            .state
            .store(TaskState::Idle as u8, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_flight_blocks_second_begin() {
        let g = TaskGate::new();
        g.begin_waiting().unwrap();
        assert!(g.begin_waiting().is_err());
    }

    #[test]
    fn double_fire_blocked() {
        let g = TaskGate::new();
        g.begin_waiting().unwrap();
        g.set_armed();
        assert!(g.try_begin_firing());
        assert!(!g.try_begin_firing());
        g.finish_success();
        assert_eq!(g.state(), TaskState::Success);
    }

    #[test]
    fn cancel_prevents_firing() {
        let g = TaskGate::new();
        g.begin_waiting().unwrap();
        g.request_cancel();
        assert!(!g.try_begin_firing());
        assert_eq!(g.state(), TaskState::Cancelled);
    }
}

/// Process-global cancel flag used by countdown so Telegram Cancel can abort waits.
fn global_cancel() -> &'static AtomicBool {
    use std::sync::OnceLock;
    static G: OnceLock<AtomicBool> = OnceLock::new();
    G.get_or_init(|| AtomicBool::new(false))
}

pub fn clear_global_cancel() {
    global_cancel().store(false, Ordering::Release);
}

pub fn request_global_cancel() {
    global_cancel().store(true, Ordering::Release);
}

pub fn global_cancelled() -> bool {
    global_cancel().load(Ordering::Acquire)
}

pub fn global_cancel_flag() -> &'static AtomicBool {
    global_cancel()
}
