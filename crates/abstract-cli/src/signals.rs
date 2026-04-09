//! Signal handling: Ctrl+C (single = cancel, double = exit), SIGTERM.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

static LAST_CTRLC: parking_lot::Mutex<Option<Instant>> = parking_lot::Mutex::new(None);

/// A handle to the active cancellation token. The signal handler cancels whatever
/// token is currently stored here. Call [`SignalHandle::reset`] after each cancelled
/// turn to install a fresh token so the next run is not pre-cancelled.
pub struct SignalHandle {
    active: Arc<parking_lot::Mutex<CancellationToken>>,
}

impl SignalHandle {
    /// Return a clone of the current active token (for passing to the agent).
    pub fn token(&self) -> CancellationToken {
        self.active.lock().clone()
    }

    /// Replace the active token with a brand-new one and return it.
    pub fn reset(&self) -> CancellationToken {
        let new = CancellationToken::new();
        *self.active.lock() = new.clone();
        new
    }
}

/// Install signal handlers. Returns a [`SignalHandle`] whose token can be reset
/// between turns to allow cancellation to be used more than once per session.
pub fn install(running: Arc<AtomicBool>) -> anyhow::Result<SignalHandle> {
    let handle = SignalHandle {
        active: Arc::new(parking_lot::Mutex::new(CancellationToken::new())),
    };
    let active = Arc::clone(&handle.active);
    let r = running.clone();

    ctrlc_handler(move || {
        let mut last = LAST_CTRLC.lock();
        let now = Instant::now();

        // Double Ctrl+C within 500ms = hard exit
        if let Some(prev) = *last {
            if now.duration_since(prev).as_millis() < 500 {
                eprintln!("\nForce exit.");
                std::process::exit(130);
            }
        }
        *last = Some(now);

        if r.load(Ordering::Relaxed) {
            // Agent is running — cancel the current turn's token
            active.lock().cancel();
            eprintln!("\n  Cancelling... (press Ctrl+C again to force exit)");
        } else {
            // Not running — exit
            eprintln!("\nGoodbye.");
            std::process::exit(0);
        }
    });

    Ok(handle)
}

fn ctrlc_handler(f: impl Fn() + Send + 'static) {
    let _ = ctrlc::set_handler(f);
}
