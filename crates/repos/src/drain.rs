//! Graceful termination, as the control plane's "Graceful termination" section
//! of the documentation site states it for every service.
//!
//! Told to stop, the service fails its readiness probe at once, so its endpoint
//! leaves the `Service`; keeps accepting for a further [`DRAIN_DELAY`], so a
//! request routed by an endpoint list not yet updated still lands; then stops
//! accepting and finishes every request in flight, a bundle upload, a download
//! or a publish among them, before it exits with status 0. A request still in
//! flight at the [`STOP_DEADLINE`] after the signal is abandoned, so the exit
//! stays clean and inside the pod's grace. An interrupt at a terminal skips the
//! drain delay. The service serves no long-lived stream, so there is none to
//! close.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::Instant;

/// How long the service keeps accepting after it was told to stop, its
/// readiness probe answering `503` meanwhile.
pub const DRAIN_DELAY: Duration = Duration::from_secs(5);

/// How long after the signal the service exits however much is still in
/// flight: the pod's `300s` grace less 10 seconds. The grace counts from the
/// start of the 5-second `preStop` hook that precedes `SIGTERM`, so the
/// deadline falls 5 seconds before the kubelet would kill the process.
pub const STOP_DEADLINE: Duration = Duration::from_secs(290);

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

/// Install the handlers for `SIGTERM` and the interrupt, and answer a future
/// that resolves with the first of them to arrive.
///
/// The handlers are installed by this call, not when the future is first
/// polled, so a signal delivered at any point afterwards is held for the future
/// rather than ending the process by the signal's default action. Call it
/// before anything that can take time, so a stop during a slow start is never
/// lost.
pub fn stop_signal() -> std::io::Result<impl Future<Output = Stop> + Send + 'static> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        Ok(async move {
            tokio::select! {
                _ = terminate.recv() => Stop::Terminate,
                _ = interrupt.recv() => Stop::Interrupt,
            }
        })
    }
    #[cfg(not(unix))]
    {
        Ok(async move {
            // Only an interrupt reaches a process here. An error listening for
            // it leaves no way to be asked to stop, so wait forever then.
            match tokio::signal::ctrl_c().await {
                Ok(()) => Stop::Interrupt,
                Err(err) => {
                    tracing::warn!(error = %err, "could not listen for an interrupt");
                    std::future::pending().await
                }
            }
        })
    }
}

/// The durations of a drain: [`DRAIN_DELAY`] and [`STOP_DEADLINE`] in the
/// service, shorter ones in a test.
#[derive(Clone, Copy, Debug)]
pub struct Durations {
    /// How long the service keeps accepting after a `SIGTERM`.
    pub drain_delay: Duration,
    /// How long after the signal the service stops waiting for what is in
    /// flight.
    pub stop_deadline: Duration,
}

impl Default for Durations {
    fn default() -> Self {
        Self {
            drain_delay: DRAIN_DELAY,
            stop_deadline: STOP_DEADLINE,
        }
    }
}

/// How a served service ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// Every request in flight finished.
    Drained,
    /// The stop deadline passed with requests still in flight, which were
    /// abandoned.
    Abandoned,
}

/// Serve `router` on `listener` until `stop` resolves, then drain: mark
/// `draining` at once, keep accepting for the drain delay unless the stop was
/// an interrupt, then stop accepting and finish every request in flight,
/// giving up on them at the stop deadline after the signal. Fails only when the
/// server does.
pub async fn serve(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    draining: Draining,
    stop: impl Future<Output = Stop> + Send + 'static,
    durations: Durations,
) -> std::io::Result<Stopped> {
    // The moment the stop was requested, from which the stop deadline runs.
    let (signalled_tx, signalled_rx) = oneshot::channel::<Instant>();
    let shutdown = async move {
        let stop = stop.await;
        let _ = signalled_tx.send(Instant::now());
        draining.mark();
        match stop {
            Stop::Terminate => {
                tracing::info!(
                    delay_secs = durations.drain_delay.as_secs_f64(),
                    "stopping: readiness failed; accepting through the drain delay"
                );
                tokio::time::sleep(durations.drain_delay).await;
            }
            Stop::Interrupt => tracing::info!("interrupted: skipping the drain delay"),
        }
        tracing::info!("stopping: no longer accepting; finishing the requests in flight");
    };
    let served = axum::serve(listener, router).with_graceful_shutdown(shutdown);
    let deadline = async move {
        match signalled_rx.await {
            Ok(signalled) => tokio::time::sleep_until(signalled + durations.stop_deadline).await,
            // The server ended without a stop being requested: only its own
            // result decides.
            Err(_) => std::future::pending().await,
        }
    };
    tokio::select! {
        result = served => result.map(|()| Stopped::Drained),
        () = deadline => {
            tracing::warn!(
                deadline_secs = durations.stop_deadline.as_secs_f64(),
                "the stop deadline passed with requests still in flight; abandoning them"
            );
            Ok(Stopped::Abandoned)
        }
    }
}

#[cfg(test)]
#[path = "drain.test.rs"]
mod tests;
