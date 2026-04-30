//! Shared terminal-input coordination for interactive prompts.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

static TERMINAL_INPUT_LOCK: Mutex<()> = Mutex::new(());
static PROMPT_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) struct PromptActiveGuard;

impl Drop for PromptActiveGuard {
    fn drop(&mut self) {
        PROMPT_ACTIVE.store(false, Ordering::SeqCst);
    }
}

pub(crate) fn prompt_active_guard() -> PromptActiveGuard {
    PROMPT_ACTIVE.store(true, Ordering::SeqCst);
    PromptActiveGuard
}

pub(crate) fn prompt_active() -> bool {
    PROMPT_ACTIVE.load(Ordering::SeqCst)
}

pub(crate) fn with_input_lock<T>(f: impl FnOnce() -> T) -> T {
    let _guard = TERMINAL_INPUT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f()
}

pub(crate) fn try_input_lock() -> Option<MutexGuard<'static, ()>> {
    TERMINAL_INPUT_LOCK.try_lock().ok()
}
