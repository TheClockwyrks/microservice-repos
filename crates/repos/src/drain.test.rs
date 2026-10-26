use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

use super::*;

/// How long any one wait may take before the test fails.
const PATIENCE: Duration = Duration::from_secs(30);

/// A router whose `/slow` answers once `release` lets it and whose `/stuck`
/// never answers, each telling `entered` when a request reaches it.
fn router(entered: mpsc::UnboundedSender<()>, release: oneshot::Receiver<()>) -> Router {
    let release = std::sync::Arc::new(tokio::sync::Mutex::new(Some(release)));
    let stuck = entered.clone();
    Router::new()
        .route(
            "/slow",
            get(move || {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    let _ = entered.send(());
                    if let Some(release) = release.lock().await.take() {
                        let _ = release.await;
                    }
                    "done"
                }
            }),
        )
        .route(
            "/stuck",
            get(move || {
                let _ = stuck.send(());
                std::future::pending::<&'static str>()
            }),
        )
}

/// Send `GET <path>` on a fresh connection, answering the stream to read the
/// response from.
async fn request(address: SocketAddr, path: &str) -> TcpStream {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nhost: {address}\r\nconnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    stream
}

async fn start(
    router: Router,
    durations: Durations,
) -> (
    SocketAddr,
    Draining,
    oneshot::Sender<Stop>,
    tokio::task::JoinHandle<std::io::Result<Stopped>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let draining = Draining::new();
    let (tell, told) = oneshot::channel();
    let served = tokio::spawn(serve(
        listener,
        router,
        draining.clone(),
        async move { told.await.unwrap() },
        durations,
    ));
    (address, draining, tell, served)
}

#[tokio::test]
async fn a_terminate_drains_at_once_waits_out_the_delay_and_finishes_what_is_in_flight() {
    let (entered_tx, mut entered) = mpsc::unbounded_channel();
    let (release, released) = oneshot::channel();
    let delay = Duration::from_millis(300);
    let durations = Durations {
        drain_delay: delay,
        stop_deadline: PATIENCE,
    };
    let (address, draining, tell, served) = start(router(entered_tx, released), durations).await;
    let mut slow = request(address, "/slow").await;
    tokio::time::timeout(PATIENCE, entered.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(!draining.is_draining());

    let signalled = Instant::now();
    tell.send(Stop::Terminate).unwrap();
    let deadline = Instant::now() + PATIENCE;
    while !draining.is_draining() {
        assert!(Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    // Wait out the delay and then some: the server still waits on the request.
    tokio::time::sleep(delay * 2).await;
    assert!(!served.is_finished());
    assert!(signalled.elapsed() >= delay);

    release.send(()).unwrap();
    let mut response = String::new();
    slow.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("done"), "{response}");
    let stopped = tokio::time::timeout(PATIENCE, served)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(stopped, Stopped::Drained);
}

#[tokio::test]
async fn an_interrupt_skips_the_delay() {
    let (entered_tx, _entered) = mpsc::unbounded_channel();
    let (_release, released) = oneshot::channel();
    let durations = Durations {
        drain_delay: Duration::from_secs(600),
        stop_deadline: Duration::from_secs(600),
    };
    let (_, draining, tell, served) = start(router(entered_tx, released), durations).await;
    tell.send(Stop::Interrupt).unwrap();
    let stopped = tokio::time::timeout(PATIENCE, served)
        .await
        .expect("an interrupt waited out the drain delay")
        .unwrap()
        .unwrap();
    assert_eq!(stopped, Stopped::Drained);
    assert!(draining.is_draining());
}

#[tokio::test]
async fn a_request_still_in_flight_at_the_stop_deadline_is_abandoned() {
    let (entered_tx, mut entered) = mpsc::unbounded_channel();
    let (_release, released) = oneshot::channel();
    let durations = Durations {
        drain_delay: Duration::from_millis(50),
        stop_deadline: Duration::from_millis(400),
    };
    let (address, _, tell, served) = start(router(entered_tx, released), durations).await;
    let _stuck = request(address, "/stuck").await;
    tokio::time::timeout(PATIENCE, entered.recv())
        .await
        .unwrap()
        .unwrap();

    let signalled = Instant::now();
    tell.send(Stop::Terminate).unwrap();
    let stopped = tokio::time::timeout(PATIENCE, served)
        .await
        .expect("the stop deadline did not end the serve")
        .unwrap()
        .unwrap();
    assert_eq!(stopped, Stopped::Abandoned);
    assert!(signalled.elapsed() >= durations.stop_deadline);
}

#[test]
fn the_stop_deadline_leaves_the_grace_ten_seconds() {
    let defaults = Durations::default();
    assert_eq!(defaults.drain_delay, Duration::from_secs(5));
    assert_eq!(
        defaults.stop_deadline,
        Duration::from_secs(300) - Duration::from_secs(10)
    );
}
