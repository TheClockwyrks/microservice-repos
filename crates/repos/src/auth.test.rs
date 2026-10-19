//! The token checks against the stub backend.

use axum::http::{HeaderMap, HeaderValue, StatusCode};

use super::*;
use crate::testing::{
    BROKEN_JOB_ID, JOB_ID, JOB_TOKEN, PUBLISH_JOB_ID, PUBLISH_TOKEN, serve_backend,
};

fn bearer_headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    headers
}

#[tokio::test]
async fn a_job_token_is_verified_against_its_job() {
    let (backend, stub) = serve_backend().await;
    let http = reqwest::Client::new();
    verify_job_token(&http, &backend, JOB_ID, JOB_TOKEN)
        .await
        .unwrap();
    let wrong = verify_job_token(&http, &backend, JOB_ID, "other")
        .await
        .unwrap_err();
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    let unknown = verify_job_token(&http, &backend, "job-9", JOB_TOKEN)
        .await
        .unwrap_err();
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
    let empty = verify_job_token(&http, &backend, JOB_ID, "")
        .await
        .unwrap_err();
    assert_eq!(empty.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        stub.lock().unwrap().verifies,
        ["job:job-1", "job:job-1", "job:job-9"]
    );
}

#[tokio::test]
async fn a_publish_job_token_is_verified_against_the_publish_job_endpoint() {
    let (backend, stub) = serve_backend().await;
    let http = reqwest::Client::new();
    verify_publish_job_token(&http, &backend, PUBLISH_JOB_ID, PUBLISH_TOKEN)
        .await
        .unwrap();
    // A driver's token is not a publisher's.
    let err = verify_publish_job_token(&http, &backend, JOB_ID, JOB_TOKEN)
        .await
        .unwrap_err();
    assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        stub.lock().unwrap().verifies,
        ["publish-job:publish-1", "publish-job:job-1"]
    );
}

#[tokio::test]
async fn an_unavailable_backend_is_a_bad_gateway_not_a_refusal() {
    let (backend, _stub) = serve_backend().await;
    let http = reqwest::Client::new();
    let failing = verify_job_token(&http, &backend, BROKEN_JOB_ID, JOB_TOKEN)
        .await
        .unwrap_err();
    assert_eq!(failing.status, StatusCode::BAD_GATEWAY);
    assert_eq!(failing.code, "auth_unavailable");
    let unreachable = verify_job_token(&http, "http://127.0.0.1:1", JOB_ID, JOB_TOKEN)
        .await
        .unwrap_err();
    assert_eq!(unreachable.status, StatusCode::BAD_GATEWAY);
}

#[test]
fn the_service_token_is_compared_exactly() {
    verify_service_token(&bearer_headers("svc"), Some("svc")).unwrap();
    for (headers, expected) in [
        (bearer_headers("svd"), Some("svc")),
        (bearer_headers("svc2"), Some("svc")),
        (HeaderMap::new(), Some("svc")),
        (bearer_headers("svc"), None),
    ] {
        let err = verify_service_token(&headers, expected).unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }
}

#[test]
fn bearer_reads_a_non_empty_bearer_credential() {
    assert_eq!(bearer(&bearer_headers(" t ")).as_deref(), Some("t"));
    let mut basic = HeaderMap::new();
    basic.insert("authorization", HeaderValue::from_static("Basic abc"));
    assert_eq!(bearer(&basic), None);
    assert_eq!(bearer(&bearer_headers("")), None);
}

#[test]
fn a_job_id_header_is_required() {
    let mut headers = HeaderMap::new();
    assert_eq!(
        job_id(&headers, JOB_ID_HEADER).unwrap_err().status,
        StatusCode::UNAUTHORIZED
    );
    headers.insert(JOB_ID_HEADER, HeaderValue::from_static(" job-1 "));
    assert_eq!(job_id(&headers, JOB_ID_HEADER).unwrap(), "job-1");
}
