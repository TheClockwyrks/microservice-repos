use std::time::{Duration, Instant};

use super::*;

#[tokio::test]
async fn a_terminate_drains_at_once_and_waits_out_the_delay() {
    let draining = Draining::new();
    assert!(!draining.is_draining());
    let (tell, told) = tokio::sync::oneshot::channel();
    let delay = Duration::from_millis(300);
    let drain = tokio::spawn(drain_on(
        async move { told.await.unwrap() },
        draining.clone(),
        delay,
    ));
    // Not told to stop, the service does not drain.
    tokio::task::yield_now().await;
    assert!(!draining.is_draining());

    let started = Instant::now();
    tell.send(Stop::Terminate).unwrap();
    // Readiness fails well before the delay ends.
    while !draining.is_draining() {
        tokio::task::yield_now().await;
    }
    assert!(!drain.is_finished());
    drain.await.unwrap();
    assert!(started.elapsed() >= delay);
    assert!(draining.is_draining());
}

#[tokio::test]
async fn an_interrupt_skips_the_delay() {
    let draining = Draining::new();
    let started = Instant::now();
    drain_on(
        async { Stop::Interrupt },
        draining.clone(),
        Duration::from_secs(600),
    )
    .await;
    assert!(draining.is_draining());
    assert!(started.elapsed() < Duration::from_secs(60));
}
