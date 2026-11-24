use std::{
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

/// A signal shared between the execution and the host requesting cancellation.
#[derive(Clone, Default)]
pub struct CancelSignal {
    state: Arc<State>,
}

#[derive(Default)]
struct State {
    cancelled: Mutex<bool>,
    wake: Condvar,
}

impl CancelSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        *self.lock() = true;
        self.state.wake.notify_all();
    }

    pub fn is_cancelled(&self) -> bool {
        *self.lock()
    }

    /// Returns whether cancellation was signaled before the wait ended.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let (cancelled, _) = self
            .state
            .wake
            .wait_timeout_while(self.lock(), timeout, |cancelled| !*cancelled)
            .unwrap_or_else(PoisonError::into_inner);
        *cancelled
    }

    fn lock(&self) -> MutexGuard<'_, bool> {
        self.state
            .cancelled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}
