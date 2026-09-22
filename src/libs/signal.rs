use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};

use nagoya::sync::Notify;
use tokio::signal::unix::{Signal, SignalKind, signal};

/// A flag every waiter observes, including one that arrives after the cancel.
///
/// `Notify::notify_waiters` wakes the current set and leaves no permit. That
/// is nagoya 0.1.9 (`sync.rs`), the version this crate pins. A waiter that
/// starts later reads [`Shutdown::is_cancelled`] instead of taking a stored
/// wake, which is the half `CancellationToken::cancel` was providing.
pub struct Shutdown {
    cancelled: AtomicBool,
    notify: Notify,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    /// A flag that has not been cancelled.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    /// Store the flag, then wake everyone currently waiting.
    ///
    /// The store comes first. A waiter that observes the broadcast and then
    /// reads the flag has to see `true`, and `notify_waiters` keeps no permit
    /// that could cover the other order.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Whether [`Shutdown::cancel`] has run.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Resolves once [`Shutdown::cancel`] has run.
    ///
    /// The wait is registered before the flag is read. A cancel landing
    /// between the two would otherwise be lost, because the broadcast keeps
    /// no permit. This is the same order nagoya's `Barrier` uses.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            let mut notified = pin!(notified);
            if notified.as_mut().enable() {
                if self.is_cancelled() {
                    return;
                }
                continue;
            }
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// Process-wide shutdown flag. Delivery of the unix signals is still tokio;
/// this is only the flag those waits set.
pub static CANCELLATION_TOKEN: Shutdown = Shutdown::new();

/// initialize and return the signals (sigterm, sigint)
pub fn init_signals() -> eyre::Result<(Signal, Signal)> {
    let sigterm = signal(SignalKind::terminate())?;
    let sigint = signal(SignalKind::interrupt())?;
    Ok((sigterm, sigint))
}

// async function to wait for the signals
pub async fn wait_for_signals(sigterm: &mut Signal, sigint: &mut Signal) {
    // SIGTERM is polled first. Both arms set the same flag; the name in the log
    // is the only difference, and a pending SIGTERM is the one we report.
    let term = sigterm.recv();
    let int = sigint.recv();
    futures::pin_mut!(term, int);
    match futures::future::select(term, int).await {
        futures::future::Either::Left(_) => inform_terminate("SIGTERM"),
        futures::future::Either::Right(_) => inform_terminate("SIGINT"),
    }
}

// async function to wait for the signals
pub async fn signal_received_silent() {
    let mut sigterm = signal(SignalKind::terminate()).expect("");
    let mut sigint = signal(SignalKind::interrupt()).expect("");
    let term = sigterm.recv();
    let int = sigint.recv();
    futures::pin_mut!(term, int);
    let _ = futures::future::select(term, int).await;
}

/// print external signal
fn inform_terminate(signal_alias: &str) {
    if !get_terminate_flag() {
        tracing::warn!("received {signal_alias} signal, terminating program");
        set_terminate_flag()
    }
}

pub fn set_terminate_flag() {
    CANCELLATION_TOKEN.cancel();
}

pub fn get_terminate_flag() -> bool {
    CANCELLATION_TOKEN.is_cancelled()
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::Context;

    use super::*;

    #[test]
    fn cancel_completes_a_waiter_that_already_parked() {
        let shutdown = Shutdown::new();
        let mut fut = pin!(shutdown.cancelled());
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert!(!shutdown.is_cancelled());

        shutdown.cancel();

        assert!(shutdown.is_cancelled());
        assert!(fut.as_mut().poll(&mut cx).is_ready());
    }

    #[test]
    fn a_waiter_that_starts_after_cancel_completes_immediately() {
        let shutdown = Shutdown::new();
        shutdown.cancel();

        let mut fut = pin!(shutdown.cancelled());
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(shutdown.is_cancelled());
        assert!(fut.as_mut().poll(&mut cx).is_ready());
    }
}
