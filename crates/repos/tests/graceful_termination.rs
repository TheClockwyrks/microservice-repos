//! The `tcab-repos` binary's graceful termination, end to end: the real process,
//! sent a real signal, over a stub backend and a stub Blob service on loopback.
//! See the control plane's "Graceful termination" in the documentation site.
//!
//! The upload in flight is written by hand over a socket, so the test decides
//! when its body arrives: half before the `SIGTERM`, the rest only once the
//! service has stopped accepting, so the request is provably in flight across
//! the whole drain.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode, Uri};
use axum::routing::post;
use test_cabinet_repos::drain::DRAIN_DELAY;
use test_cabinet_repos::git::Git;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// The run the upload is for.
const RUN: &str = "d483a2f9-1111-2222-3333-444455556666";
/// The job the driver's token belongs to.
const JOB_ID: &str = "job-1";
/// Azurite's well-known account key (public, documented by the emulator); the
/// stub accepts any signature, but the service needs a base64 key to sign with.
const KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
/// How long any one wait in the test may take before it fails: far beyond what
/// each step needs, so a slow machine does not fail it.
const PATIENCE: Duration = Duration::from_secs(30);

/// What the stubs saw.
#[derive(Default)]
struct Seen {
    /// The job ids whose token the stub backend verified.
    verifies: Vec<String>,
    /// The blobs the stub Blob service holds, by path.
    blobs: BTreeMap<String, Bytes>,
    /// How many requests the stub Blob service received.
    blob_requests: usize,
    /// Whether the stub Blob service answers every request `503`, as a store
    /// still coming up does.
    blob_unavailable: bool,
}

type Shared = Arc<Mutex<Seen>>;

/// Serve `router` on a loopback port, answering its origin.
async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

/// A backend that accepts every driver token, recording the job it names.
async fn serve_backend(seen: Shared) -> String {
    async fn verify(
        State(seen): State<Shared>,
        axum::extract::Path(job): axum::extract::Path<String>,
    ) -> StatusCode {
        seen.lock().unwrap().verifies.push(job);
        StatusCode::OK
    }
    serve(
        Router::new()
            .route("/jobs/{job}/verify-token", post(verify))
            .with_state(seen),
    )
    .await
}

/// A Blob service that accepts any credential: it creates the container,
/// answers the start's listing, and keeps every blob put to it.
async fn serve_blob(seen: Shared) -> String {
    async fn blob(State(seen): State<Shared>, method: Method, uri: Uri, body: Bytes) -> StatusCode {
        let query = uri.query().unwrap_or("");
        {
            let mut seen = seen.lock().unwrap();
            seen.blob_requests += 1;
            if seen.blob_unavailable {
                return StatusCode::SERVICE_UNAVAILABLE;
            }
        }
        match (method, query.contains("restype=container")) {
            (Method::PUT, true) => StatusCode::CREATED,
            (Method::GET, true) => StatusCode::OK,
            (Method::PUT, false) => {
                seen.lock()
                    .unwrap()
                    .blobs
                    .insert(uri.path().to_string(), body);
                StatusCode::CREATED
            }
            _ => StatusCode::NOT_IMPLEMENTED,
        }
    }
    serve(Router::new().fallback(blob).with_state(seen)).await
}

/// A free loopback port for the service to bind.
fn free_port() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// The running binary, killed if the test ends before it exits.
struct Service {
    child: Child,
    address: SocketAddr,
    _cwd: tempfile::TempDir,
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Service {
    /// Start `tcab-repos` against the stubs and wait until it is ready.
    async fn start(backend: &str, blob: &str) -> Self {
        let service = Self::spawn(backend, blob);
        service
            .wait_for("the service to become ready", |probe| {
                probe == Probe::Status(200)
            })
            .await;
        service
    }

    /// Spawn `tcab-repos` against the stubs, not waiting for it.
    fn spawn(backend: &str, blob: &str) -> Self {
        let address = free_port();
        // A working directory of its own, so no `.env.repos` is read.
        let cwd = tempfile::tempdir().unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_tcab-repos"))
            .current_dir(cwd.path())
            .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
            .env_remove("TCAB_GITHUB_ORG")
            .env_remove("TCAB_GITHUB_TOKEN")
            .env_remove("TCAB_BACKEND_SERVICE_TOKEN")
            .env("TCAB_REPOS_BIND", address.to_string())
            .env("TCAB_BACKEND_URL", backend)
            .env("TCAB_REPOS_BLOB_ACCOUNT", "devstoreaccount1")
            .env("TCAB_REPOS_BLOB_CONTAINER", "bundles")
            .env(
                "TCAB_REPOS_BLOB_ENDPOINT",
                format!("{blob}/devstoreaccount1"),
            )
            .env("TCAB_REPOS_BLOB_KEY", KEY)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        Self {
            child,
            address,
            _cwd: cwd,
        }
    }

    /// Send the process `signal` (`TERM` or `INT`).
    fn signal(&self, signal: &str) {
        let status = Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        assert!(status.success(), "kill -{signal} failed");
    }

    /// Ask `/readyz` once, on a fresh connection.
    async fn probe(&self) -> Probe {
        let Ok(mut stream) = TcpStream::connect(self.address).await else {
            return Probe::Refused;
        };
        let request = format!(
            "GET /readyz HTTP/1.1\r\nhost: {}\r\nconnection: close\r\n\r\n",
            self.address
        );
        if stream.write_all(request.as_bytes()).await.is_err() {
            return Probe::Refused;
        }
        // A connection queued as the listener closed is reset, or closed with
        // no answer: both mean the service no longer accepts.
        let mut response = Vec::new();
        if stream.read_to_end(&mut response).await.is_err() {
            return Probe::Refused;
        }
        status_of(&response).map_or(Probe::Refused, Probe::Status)
    }

    /// Probe `/readyz` until `done` holds of an answer, panicking after
    /// [`PATIENCE`] or if the process exits first.
    async fn wait_for(&self, what: &str, done: impl Fn(Probe) -> bool) -> Probe {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let probe = self.probe().await;
            if done(probe) {
                return probe;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Wait for the process to exit, panicking after [`PATIENCE`].
    async fn exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the service did not exit");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

/// One answer of the readiness probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Probe {
    /// No answer: nothing accepts on the port, or a connection queued as the
    /// listener closed was reset or closed unanswered.
    Refused,
    /// The HTTP status it answered.
    Status(u16),
}

/// The status code of a raw HTTP/1.1 response, or `None` for no response.
fn status_of(response: &[u8]) -> Option<u16> {
    let text = String::from_utf8_lossy(response);
    let mut words = text.split_whitespace();
    if !words.next()?.starts_with("HTTP/") {
        return None;
    }
    words.next()?.parse().ok()
}

/// A run's repository as the driver leaves it, written with `git bundle create
/// --all` under `dir`. Answers the bundle's bytes.
async fn make_bundle(dir: &Path) -> Vec<u8> {
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let git = Git::new(&work, dir).with_env(
        [
            ("GIT_AUTHOR_NAME", "Clockwyrks"),
            ("GIT_AUTHOR_EMAIL", "runs@clockwyrks.invalid"),
            ("GIT_COMMITTER_NAME", "Clockwyrks"),
            ("GIT_COMMITTER_EMAIL", "runs@clockwyrks.invalid"),
        ]
        .map(|(key, value)| (key.to_string(), value.to_string())),
    );
    git.run(&["init", "--quiet", "--initial-branch", "main"])
        .await
        .unwrap();
    std::fs::write(work.join("README.md"), "# Pong\n").unwrap();
    git.run(&["add", "--all"]).await.unwrap();
    git.run(&["commit", "--quiet", "--message", "Seed test case"])
        .await
        .unwrap();
    std::fs::write(work.join("main.js"), "console.log('pong');\n").unwrap();
    git.run(&["add", "--all"]).await.unwrap();
    git.run(&["commit", "--quiet", "--message", "Implement test case"])
        .await
        .unwrap();
    let bundle: PathBuf = dir.join("run.bundle");
    git.run(&[
        "bundle",
        "create",
        "--quiet",
        &bundle.display().to_string(),
        "--all",
    ])
    .await
    .unwrap();
    std::fs::read(bundle).unwrap()
}

#[tokio::test]
async fn sigterm_fails_readiness_drains_and_finishes_an_upload_in_flight() {
    let seen = Shared::default();
    let backend = serve_backend(seen.clone()).await;
    let blob = serve_blob(seen.clone()).await;
    let scratch = tempfile::tempdir().unwrap();
    let bundle = make_bundle(scratch.path()).await;
    let mut service = Service::start(&backend, &blob).await;

    // An upload whose body has half arrived: the handler has verified the token
    // and is staging the body when the signal comes.
    let mut upload = TcpStream::connect(service.address).await.unwrap();
    let head = format!(
        "POST /runs/{RUN}/bundle HTTP/1.1\r\nhost: {}\r\nauthorization: Bearer job-token\r\n\
         x-tcab-job-id: {JOB_ID}\r\ncontent-type: application/octet-stream\r\n\
         content-length: {}\r\n\r\n",
        service.address,
        bundle.len()
    );
    let (first, rest) = bundle.split_at(bundle.len() / 2);
    upload.write_all(head.as_bytes()).await.unwrap();
    upload.write_all(first).await.unwrap();
    upload.flush().await.unwrap();
    let deadline = Instant::now() + PATIENCE;
    while seen.lock().unwrap().verifies.is_empty() {
        assert!(
            Instant::now() < deadline,
            "the upload never reached its handler"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // The service starts its drain delay when the signal lands, which is before
    // `kill` returns, so the clock starts before it is sent.
    let signalled = Instant::now();
    service.signal("TERM");

    // Readiness fails at once, while the service still accepts: the probe is
    // answered `503` rather than refused.
    service
        .wait_for("readiness to fail", |probe| probe != Probe::Status(200))
        .await;
    assert_eq!(service.probe().await, Probe::Status(503));
    assert!(
        signalled.elapsed() < DRAIN_DELAY,
        "readiness did not fail within the drain delay"
    );

    // Past the drain delay, the service stops accepting.
    service
        .wait_for("the service to stop accepting", |probe| {
            probe == Probe::Refused
        })
        .await;
    assert!(
        signalled.elapsed() >= DRAIN_DELAY,
        "the service stopped accepting before the drain delay ended"
    );
    assert!(
        service.child.try_wait().unwrap().is_none(),
        "the service exited with an upload in flight"
    );

    // The upload in flight still completes and is stored.
    upload.write_all(rest).await.unwrap();
    upload.flush().await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(PATIENCE, upload.read_to_end(&mut response))
        .await
        .expect("the upload was never answered")
        .unwrap();
    assert_eq!(
        status_of(&response),
        Some(201),
        "{}",
        String::from_utf8_lossy(&response)
    );
    let stored = seen
        .lock()
        .unwrap()
        .blobs
        .get(&format!("/devstoreaccount1/bundles/{RUN}.bundle"))
        .cloned();
    assert_eq!(stored.as_deref(), Some(bundle.as_slice()));

    // With nothing left in flight, it exits cleanly.
    let status = service.exit().await;
    assert!(status.success(), "the service exited with {status}");
}

#[tokio::test]
async fn an_interrupt_skips_the_drain_delay_and_exits_cleanly() {
    let seen = Shared::default();
    let backend = serve_backend(seen.clone()).await;
    let blob = serve_blob(seen).await;
    let mut service = Service::start(&backend, &blob).await;

    let signalled = Instant::now();
    service.signal("INT");
    let status = service.exit().await;
    assert!(status.success(), "the service exited with {status}");
    assert!(
        signalled.elapsed() < DRAIN_DELAY,
        "an interrupt waited out the drain delay"
    );
}

#[tokio::test]
async fn a_sigterm_during_the_start_exits_cleanly() {
    let seen = Shared::default();
    seen.lock().unwrap().blob_unavailable = true;
    let backend = serve_backend(seen.clone()).await;
    let blob = serve_blob(seen.clone()).await;
    let mut service = Service::spawn(&backend, &blob);

    // The start is retrying the unavailable store, so its signal handlers are
    // installed and its slow part is under way.
    let deadline = Instant::now() + PATIENCE;
    while seen.lock().unwrap().blob_requests == 0 {
        assert!(
            Instant::now() < deadline,
            "the start never reached the store"
        );
        assert!(
            service.child.try_wait().unwrap().is_none(),
            "the service exited while starting"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    service.signal("TERM");
    let status = service.exit().await;
    assert!(status.success(), "the service exited with {status}");
    assert_eq!(service.probe().await, Probe::Refused);
}
