//! Graceful termination, as the control plane's "Graceful termination" section
//! of the documentation site states it for every service.
//!
//! Told to stop, the service fails its readiness probe at once, so its endpoint
//! leaves the `Service`; keeps accepting for a further [`DRAIN_DELAY`], so a
//! request routed by an endpoint list not yet updated still lands; then stops
//! accepting and finishes every request in flight, a bundle upload, a download
//! or a publish among them, before it exits with status 0. An interrupt at a
//! terminal skips the drain delay. The service serves no long-lived stream, so
//! there is none to close.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long the service keeps accepting after it was told to stop, its
/// readiness probe answering `503` meanwhile.
pub const DRAIN_DELAY: Duration = Duration::from_secs(5);

/// Whether the service was told to stop. Cheap to clone; every clone shares one
/// flag, so the readiness probe sees what the shutdown sets. Never cleared.
#[derive(Clone, Debug, Default)]
pub struct Draining(Arc<AtomicBool>);

impl Draining {
    /// A service not told to stop.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start draining: the readiness probe fails from now on. Idempotent.
    pub fn mark(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether the service was told to stop.
    pub fn is_draining(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// What told the service to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// `SIGTERM`, which Kubernetes sends a pod it stops.
    Terminate,
    /// An interrupt (`Ctrl-C`) at a terminal, which skips the drain delay.
    Interrupt,
}

/// Resolve when the process receives `SIGTERM` or an interrupt, answering which.
pub async fn stop_requested() -> Stop {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(terminate) => terminate,
                Err(err) => {
                    tracing::warn!("cannot listen for SIGTERM ({err}); stopping on Ctrl-C only");
                    let _ = interrupt.await;
                    return Stop::Interrupt;
                }
            };
        tokio::select! {
            _ = interrupt => Stop::Interrupt,
            _ = terminate.recv() => Stop::Terminate,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = interrupt.await;
        Stop::Interrupt
    }
}

/// The future a server's graceful shutdown waits on: once `stop` resolves, mark
/// `draining` at once, then wait out `delay` unless the stop was an interrupt.
/// When it resolves the server stops accepting and finishes what is in flight.
pub async fn drain_on(stop: impl Future<Output = Stop>, draining: Draining, delay: Duration) {
    let stop = stop.await;
    draining.mark();
    match stop {
        Stop::Terminate => {
            tracing::info!(
                delay_secs = delay.as_secs_f64(),
                "stopping: readiness failed; accepting for the drain delay"
            );
            tokio::time::sleep(delay).await;
        }
        Stop::Interrupt => tracing::info!("interrupted: skipping the drain delay"),
    }
    tracing::info!("stopping: no longer accepting; finishing the requests in flight");
}

#[cfg(test)]
#[path = "drain.test.rs"]
mod tests;
