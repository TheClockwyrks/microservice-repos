//! The routes end to end over the stub Blob service, the stub backend and the
//! stub GitHub.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt as _;

use super::*;
use crate::blob::testing::{Auth, serve as serve_blob, shared_key_client};
use crate::publish::PushRetry;
use crate::testing::{
    JOB_ID, JOB_TOKEN, ORG, PUBLISH_JOB_ID, PUBLISH_TOKEN, SERVICE_TOKEN, SharedBackend,
    SharedGitHub, make_bundle, record, serve_backend, serve_github,
};

const RUN: &str = "d483a2f9-1111-2222-3333-444455556666";

struct Harness {
    state: AppState,
    backend: SharedBackend,
    blob: crate::blob::testing::Shared,
    github: SharedGitHub,
    root: tempfile::TempDir,
    work: tempfile::TempDir,
}

async fn harness() -> Harness {
    let (origin, blob) = serve_blob(Auth::SharedKey).await;
    let client = shared_key_client(&origin, "bundles");
    client.create_container().await.unwrap();
    let (backend_url, backend) = serve_backend().await;
    let root = tempfile::tempdir().unwrap();
    let (github_config, github) = serve_github(root.path()).await;
    let publisher = Publisher::new(github_config, reqwest::Client::new()).with_retry(PushRetry {
        attempts: 2,
        initial_delay: Duration::ZERO,
        backoff: Duration::from_millis(1),
    });
    Harness {
        state: AppState {
            blob: client,
            http: reqwest::Client::new(),
            backend_url: Arc::new(backend_url),
            service_token: Some(Arc::new(SERVICE_TOKEN.to_string())),
            publisher: Some(Arc::new(publisher)),
            upload_limit: bundle::MAX_BUNDLE_BYTES,
        },
        backend,
        blob,
        github,
        root,
        work: tempfile::tempdir().unwrap(),
    }
}

async fn send(state: &AppState, request: Request<Body>) -> (StatusCode, Vec<u8>, HeaderMap) {
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, body, headers)
}

fn upload_request(id: &str, token: &str, job: &str, body: Vec<u8>) -> Request<Body> {
    Request::post(format!("/runs/{id}/bundle"))
        .header("authorization", format!("Bearer {token}"))
        .header(JOB_ID_HEADER, job)
        .body(Body::from(body))
        .unwrap()
}

fn service_request(method: &str, id: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(format!("/runs/{id}/bundle"))
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn publish_request(id: &str, token: &str) -> Request<Body> {
    Request::post(format!("/runs/{id}/publish"))
        .header("authorization", format!("Bearer {token}"))
        .header(PUBLISH_JOB_ID_HEADER, PUBLISH_JOB_ID)
        .body(Body::empty())
        .unwrap()
}

fn error_code(body: &[u8]) -> String {
    let value: serde_json::Value = serde_json::from_slice(body).unwrap();
    value["error"]["code"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_bundle_round_trips_through_upload_download_and_delete() {
    let h = harness().await;
    let bundle = make_bundle(h.work.path(), &[("src/main.js", b"play();\n")]).await;
    let bytes = std::fs::read(&bundle).unwrap();

    let (status, _, _) = send(
        &h.state,
        upload_request(RUN, JOB_TOKEN, JOB_ID, bytes.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(h.backend.lock().unwrap().verifies, ["job:job-1"]);
    assert!(
        h.blob
            .lock()
            .unwrap()
            .blobs
            .contains_key(&("bundles".to_string(), format!("{RUN}.bundle")))
    );

    let (status, body, headers) = send(&h.state, service_request("GET", RUN, SERVICE_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, bytes);
    assert_eq!(headers["content-type"], "application/octet-stream");
    assert_eq!(
        headers["content-disposition"],
        format!("attachment; filename=\"{RUN}.bundle\"").as_str()
    );

    // The download is a cloneable repository holding the seed and the work.
    let downloaded = h.work.path().join("downloaded.bundle");
    std::fs::write(&downloaded, &body).unwrap();
    let clone = h.work.path().join("clone");
    let git = crate::git::Git::new(h.work.path(), h.work.path());
    git.run(&[
        "clone",
        "--quiet",
        &downloaded.display().to_string(),
        &clone.display().to_string(),
    ])
    .await
    .unwrap();
    let log = crate::git::Git::new(&clone, h.work.path())
        .run(&["log", "--format=%s", "main"])
        .await
        .unwrap()
        .text();
    assert_eq!(log, "Implement test case\nSeed test case");

    // A repeated upload replaces the bundle.
    let (status, _, _) = send(&h.state, upload_request(RUN, JOB_TOKEN, JOB_ID, bytes)).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _, _) = send(&h.state, service_request("DELETE", RUN, SERVICE_TOKEN)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = send(&h.state, service_request("DELETE", RUN, SERVICE_TOKEN)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body, _) = send(&h.state, service_request("GET", RUN, SERVICE_TOKEN)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_code(&body), "not_found");
}

#[tokio::test]
async fn an_upload_needs_the_jobs_own_token() {
    let h = harness().await;
    let (status, _, _) = send(
        &h.state,
        upload_request(RUN, "wrong", JOB_ID, b"x".to_vec()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The service token is not a job token.
    let (status, _, _) = send(
        &h.state,
        upload_request(RUN, SERVICE_TOKEN, JOB_ID, b"x".to_vec()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let missing_job = Request::post(format!("/runs/{RUN}/bundle"))
        .header("authorization", format!("Bearer {JOB_TOKEN}"))
        .body(Body::from("x"))
        .unwrap();
    let (status, _, _) = send(&h.state, missing_job).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body, _) = send(
        &h.state,
        upload_request(RUN, JOB_TOKEN, crate::testing::BROKEN_JOB_ID, b"x".to_vec()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(error_code(&body), "auth_unavailable");
    assert!(h.blob.lock().unwrap().blobs.is_empty());
}

#[tokio::test]
async fn an_upload_that_is_the_runs_own_failure_is_a_4xx() {
    let mut h = harness().await;
    let (status, body, _) = send(
        &h.state,
        upload_request(RUN, JOB_TOKEN, JOB_ID, b"not a bundle".to_vec()),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(&body), "unprocessable");

    let (status, _, _) = send(
        &h.state,
        upload_request("..%2Fescape", JOB_TOKEN, JOB_ID, b"x".to_vec()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    h.state.upload_limit = 4;
    let (status, body, _) = send(
        &h.state,
        upload_request(RUN, JOB_TOKEN, JOB_ID, b"12345".to_vec()),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error_code(&body), "payload_too_large");
    assert!(h.blob.lock().unwrap().blobs.is_empty());
}

#[tokio::test]
async fn the_download_and_the_delete_take_the_service_token_alone() {
    let mut h = harness().await;
    for method in ["GET", "DELETE"] {
        let (status, _, _) = send(&h.state, service_request(method, RUN, JOB_TOKEN)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method}");
    }
    h.state.service_token = None;
    for method in ["GET", "DELETE"] {
        let (status, _, _) = send(&h.state, service_request(method, RUN, SERVICE_TOKEN)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method}");
    }
}

#[tokio::test]
async fn a_publish_pushes_the_stored_bundle_and_answers_the_url() {
    let h = harness().await;
    h.backend
        .lock()
        .unwrap()
        .runs
        .insert(RUN.to_string(), record(RUN));
    let bundle = make_bundle(h.work.path(), &[("src/main.js", b"play();\n")]).await;
    let (status, _, _) = send(
        &h.state,
        upload_request(RUN, JOB_TOKEN, JOB_ID, std::fs::read(&bundle).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let name = "tcab-pong-codex-openai-gpt-5-4-mini-d483a2f9";
    let expected = serde_json::json!({ "url": format!("https://github.com/{ORG}/{name}") });
    let (status, body, _) = send(&h.state, publish_request(RUN, PUBLISH_TOKEN)).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        expected
    );
    assert!(h.root.path().join(ORG).join(format!("{name}.git")).is_dir());

    // A second publish Job for the same run answers the same URL.
    let (status, body, _) = send(&h.state, publish_request(RUN, PUBLISH_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        expected
    );
    assert_eq!(h.github.lock().unwrap().repos.len(), 1);
}

#[tokio::test]
async fn a_publish_of_an_asset_generation_run_creates_nothing() {
    let h = harness().await;
    let mut asset = record(RUN);
    asset.subject.test_type = test_cabinet_contracts::test_case::TestType::AssetGeneration;
    h.backend
        .lock()
        .unwrap()
        .runs
        .insert(RUN.to_string(), asset);
    let (status, body, _) = send(&h.state, publish_request(RUN, PUBLISH_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({ "url": null })
    );
    assert!(h.github.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn a_publish_answers_whose_failure_it_is() {
    let mut h = harness().await;
    // A driver's token is not a publisher's.
    let (status, _, _) = send(&h.state, publish_request(RUN, JOB_TOKEN)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The backend holds no such run.
    let (status, _, _) = send(&h.state, publish_request(RUN, PUBLISH_TOKEN)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The run exists but no bundle was stored.
    h.backend
        .lock()
        .unwrap()
        .runs
        .insert(RUN.to_string(), record(RUN));
    let (status, body, _) = send(&h.state, publish_request(RUN, PUBLISH_TOKEN)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(String::from_utf8_lossy(&body).contains("no bundle"));
    // A service with no GitHub configured publishes nothing.
    h.state.publisher = None;
    let (status, body, _) = send(&h.state, publish_request(RUN, PUBLISH_TOKEN)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error_code(&body), "unconfigured");
}

#[tokio::test]
async fn the_probes_need_no_credential() {
    let h = harness().await;
    for path in ["/healthz", "/readyz"] {
        let (status, _, _) = send(&h.state, Request::get(path).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
    }
}

#[test]
fn a_run_id_is_a_safe_object_name() {
    assert_eq!(bundle_key(RUN).unwrap(), format!("{RUN}.bundle"));
    for unsafe_id in ["", ".hidden", "a/b", "a b", "..", &"x".repeat(129)] {
        assert_eq!(
            bundle_key(unsafe_id).unwrap_err().status,
            StatusCode::BAD_REQUEST,
            "{unsafe_id}"
        );
    }
}
