//! The Blob client against the stub Blob service: every operation, a staged
//! upload, Shared Key verified from the request as received, and the
//! workload-identity token.

use std::path::PathBuf;

use super::testing::{ACCOUNT, Auth, TENANT, serve, shared_key_client};
use super::*;

fn write_temp(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

async fn read_all(stream: BlobStream) -> Vec<u8> {
    let mut body = stream.body;
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    out
}

#[tokio::test]
async fn every_operation_round_trips_under_shared_key() {
    let (origin, stub) = serve(Auth::SharedKey).await;
    let client = shared_key_client(&origin, "bundles");

    assert!(client.create_container().await.unwrap());
    assert!(!client.create_container().await.unwrap());
    client.probe().await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = write_temp(&dir, "small", b"a small bundle");
    assert_eq!(
        client
            .upload_file("run-1.bundle", &path, "application/octet-stream")
            .await
            .unwrap(),
        14
    );
    let stream = client.get("run-1.bundle").await.unwrap().unwrap();
    assert_eq!(stream.length, Some(14));
    assert_eq!(read_all(stream).await, b"a small bundle");

    let copy = dir.path().join("copy");
    assert!(client.download_file("run-1.bundle", &copy).await.unwrap());
    assert_eq!(std::fs::read(&copy).unwrap(), b"a small bundle");

    assert!(client.delete("run-1.bundle").await.unwrap());
    assert!(!client.delete("run-1.bundle").await.unwrap());
    assert!(client.get("run-1.bundle").await.unwrap().is_none());
    let missing = dir.path().join("missing");
    assert!(
        !client
            .download_file("run-1.bundle", &missing)
            .await
            .unwrap()
    );
    assert!(!missing.exists());

    let log = stub.lock().unwrap().log.clone();
    assert_eq!(
        log,
        [
            "PUT", "PUT", "GET list", "PUT", "GET", "GET", "DELETE", "DELETE", "GET", "GET"
        ]
    );
}

#[tokio::test]
async fn a_file_larger_than_a_piece_is_staged_and_committed_whole() {
    let (origin, stub) = serve(Auth::SharedKey).await;
    let client = shared_key_client(&origin, "bundles");
    client.create_container().await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let data: Vec<u8> = (0..PIECE_SIZE * 2 + 5).map(|i| (i % 251) as u8).collect();
    let path = write_temp(&dir, "large", &data);
    let written = client
        .upload_file("big.bundle", &path, "application/octet-stream")
        .await
        .unwrap();
    assert_eq!(written, data.len() as u64);

    let stub = stub.lock().unwrap();
    assert_eq!(
        stub.log,
        [
            "PUT",
            "PUT block",
            "PUT block",
            "PUT block",
            "PUT blocklist"
        ]
    );
    let stored = stub
        .blobs
        .get(&("bundles".to_string(), "big.bundle".to_string()))
        .unwrap();
    assert_eq!(stored.as_ref(), data.as_slice());
}

#[tokio::test]
async fn a_wrong_key_is_refused_by_the_store() {
    let (origin, _stub) = serve(Auth::SharedKey).await;
    let client = BlobClient::new(
        &BlobConfig {
            account: ACCOUNT.to_string(),
            container: "bundles".to_string(),
            endpoint: format!("{origin}/{ACCOUNT}"),
            auth: BlobAuth::SharedKey(b"not the key".to_vec()),
        },
        reqwest::Client::new(),
    )
    .unwrap();
    let err = client.create_container().await.unwrap_err();
    assert!(
        matches!(err, BlobError::Status { status: 403, .. }),
        "{err}"
    );
}

#[tokio::test]
async fn a_missing_container_fails_the_probe() {
    let (origin, _stub) = serve(Auth::SharedKey).await;
    let client = shared_key_client(&origin, "absent");
    let err = client.probe().await.unwrap_err();
    assert!(
        matches!(err, BlobError::Status { status: 404, .. }),
        "{err}"
    );
}

#[tokio::test]
async fn workload_identity_mints_one_token_and_reuses_it() {
    let (origin, stub) = serve(Auth::Bearer).await;
    stub.lock()
        .unwrap()
        .containers
        .insert("bundles".to_string());
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_temp(&dir, "token", b"projected-sa-token\n");
    let identity = WorkloadIdentity::new(
        "client-1".to_string(),
        TENANT.to_string(),
        token_file,
        origin.clone(),
        reqwest::Client::new(),
    );
    let client = BlobClient::with_credential(
        ACCOUNT,
        &format!("{origin}/{ACCOUNT}"),
        "bundles",
        Credential::WorkloadIdentity(Arc::new(identity)),
        reqwest::Client::new(),
    );
    client.probe().await.unwrap();
    client
        .put("a", Bytes::from_static(b"x"), "application/octet-stream")
        .await
        .unwrap();
    assert_eq!(stub.lock().unwrap().token_calls, 1);
}

#[tokio::test]
async fn workload_identity_needs_the_projected_variables() {
    let err = WorkloadIdentity::from_lookup(
        |key| (key == "AZURE_CLIENT_ID").then(|| "client".to_string()),
        reqwest::Client::new(),
    )
    .unwrap_err();
    assert!(
        matches!(err, BlobError::MissingEnv("AZURE_TENANT_ID")),
        "{err}"
    );
}

#[test]
fn block_ids_share_one_length() {
    assert_eq!(block_id(0).len(), block_id(49_999).len());
}
