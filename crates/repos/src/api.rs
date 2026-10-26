//! The HTTP surface: the bundle's upload, download and delete, and the publish.
//!
//! The path `{id}` on every route is the run record's id, and the bundle is
//! stored as one object per run, `<id>.bundle`. See the repos service's Routes
//! section in the documentation site.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;
use tower_http::trace::TraceLayer;

use crate::auth::{
    JOB_ID_HEADER, PUBLISH_JOB_ID_HEADER, bearer, job_id, verify_job_token,
    verify_publish_job_token, verify_service_token,
};
use crate::blob::{BlobClient, BlobError};
use crate::bundle::{self, BundleError};
use crate::drain::Draining;
use crate::error::ApiError;
use crate::publish::{PublishError, Publisher};

/// The content type a bundle is stored and served under.
pub const BUNDLE_CONTENT_TYPE: &str = "application/octet-stream";

/// The longest run id accepted.
const MAX_ID_LEN: usize = 128;

/// The state every handler shares.
#[derive(Clone)]
pub struct AppState {
    /// The private bundle container.
    pub blob: BlobClient,
    /// The client the backend is reached with.
    pub http: reqwest::Client,
    /// The backend base URL, without a trailing slash.
    pub backend_url: Arc<String>,
    /// The shared control-plane service token, when configured.
    pub service_token: Option<Arc<String>>,
    /// The publisher, when the service is configured to publish.
    pub publisher: Option<Arc<Publisher>>,
    /// The largest upload accepted, in bytes.
    pub upload_limit: u64,
    /// Whether the service was told to stop; `/readyz` fails once it is.
    pub draining: Draining,
}

/// The service's router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route(
            "/runs/{id}/bundle",
            post(upload).get(download).delete(delete),
        )
        .route("/runs/{id}/publish", post(publish))
        // The upload streams its body to disk and applies its own cap as it
        // reads, so the default in-memory limit does not apply.
        .layer(DefaultBodyLimit::disable())
        .layer(axum::middleware::from_fn(accept_trace))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Continue any inbound W3C trace context, so the service's spans join the
/// caller's trace (a no-op with telemetry off).
async fn accept_trace(request: Request, next: axum::middleware::Next) -> Response {
    test_cabinet_telemetry::propagation::accept_inbound(request.headers());
    next.run(request).await
}

/// The liveness probe: the process is up and serving.
async fn healthz() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "status": "ok" }))
}

/// The readiness probe: `200` while the service takes new work, `503` from the
/// moment it is told to stop, so its endpoint leaves the `Service` while the
/// requests in flight finish.
async fn readyz(State(state): State<AppState>) -> Response {
    if state.draining.is_draining() {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({ "status": "draining" })),
        )
            .into_response()
    } else {
        axum::Json(serde_json::json!({ "status": "ok" })).into_response()
    }
}

/// The object a run's bundle is stored as, after checking `id` is a safe name:
/// letters, digits, `-`, `_` and `.`, not starting with `.`.
pub fn bundle_key(id: &str) -> Result<String, ApiError> {
    let safe = !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if safe {
        Ok(format!("{id}.bundle"))
    } else {
        Err(ApiError::bad_request(format!("`{id}` is not a run id")))
    }
}

/// A Blob failure, as the store being unavailable.
fn store_error(err: BlobError) -> ApiError {
    ApiError::upstream(format!("the bundle store: {err}"))
}

/// A scratch directory for one request, removed when it is dropped.
fn scratch() -> Result<tempfile::TempDir, ApiError> {
    tempfile::Builder::new()
        .prefix("tcab-repos-")
        .tempdir()
        .map_err(|err| ApiError::internal(format!("creating a scratch directory: {err}")))
}

/// `POST /runs/{id}/bundle`: store the run's bundle, replacing any earlier one.
///
/// The driver presents its per-job token and names its job in `x-tcab-job-id`;
/// the token is verified against the backend before the body is read. The body
/// is staged on disk as it arrives and refused past the cap (`413`), then
/// checked to be a bundle carrying `main` that applies to an empty repository
/// (`422` otherwise), and written as the run's object. `201 Created` once it is
/// stored.
#[tracing::instrument(name = "repos.upload", skip(state, headers, body), fields(run.id = %id, bytes = tracing::field::Empty), err(Debug))]
async fn upload(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let key = bundle_key(&id)?;
    let token = bearer(&headers).ok_or_else(|| ApiError::unauthorized("missing job token"))?;
    let job = job_id(&headers, JOB_ID_HEADER)?;
    verify_job_token(&state.http, &state.backend_url, job, &token).await?;

    let dir = scratch()?;
    let (path, length) = bundle::stage(body, state.upload_limit, dir.path())
        .await
        .map_err(bundle_error)?;
    bundle::check(&path, dir.path())
        .await
        .map_err(bundle_error)?;
    state
        .blob
        .upload_file(&key, &path, BUNDLE_CONTENT_TYPE)
        .await
        .map_err(store_error)?;
    tracing::Span::current().record("bytes", length);
    tracing::info!(run.id = %id, bytes = length, "stored the run's bundle");
    Ok(StatusCode::CREATED.into_response())
}

/// An upload's staging or check failure, by whose failure it is.
fn bundle_error(err: BundleError) -> ApiError {
    match err {
        BundleError::TooLarge { .. } => ApiError::payload_too_large(err.to_string()),
        BundleError::Invalid(_) => ApiError::unprocessable(err.to_string()),
        BundleError::Body(_) => ApiError::bad_request(err.to_string()),
        BundleError::Io(_) => ApiError::internal(err.to_string()),
    }
}

/// `GET /runs/{id}/bundle`: the run's bundle, as `application/octet-stream`
/// under the file name `<id>.bundle`, streamed from the store. The backend
/// presents the service token. `404` when no bundle is stored.
#[tracing::instrument(name = "repos.download", skip(state, headers), fields(run.id = %id), err(Debug))]
async fn download(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    verify_service_token(&headers, state.service_token.as_deref().map(String::as_str))?;
    let key = bundle_key(&id)?;
    let blob = state
        .blob
        .get(&key)
        .await
        .map_err(store_error)?
        .ok_or_else(|| ApiError::not_found(format!("no bundle is stored for run `{id}`")))?;
    let mut response = Response::new(Body::from_stream(blob.body));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(BUNDLE_CONTENT_TYPE),
    );
    // The id was checked to hold only token characters, so it is a valid
    // header value as it stands.
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{key}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    if let Some(length) = blob.length {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    }
    Ok(response)
}

/// `DELETE /runs/{id}/bundle`: remove the run's bundle, as the backend prunes a
/// deleted run. The backend presents the service token. Idempotent: `204` whether
/// or not a bundle was stored.
#[tracing::instrument(name = "repos.delete", skip(state, headers), fields(run.id = %id), err(Debug))]
async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    verify_service_token(&headers, state.service_token.as_deref().map(String::as_str))?;
    let key = bundle_key(&id)?;
    let existed = state.blob.delete(&key).await.map_err(store_error)?;
    tracing::info!(run.id = %id, existed, "deleted the run's bundle");
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The body a publish answers.
#[derive(Debug, Serialize)]
pub struct PublishOut {
    /// The run's public repository, or `null` for a run that releases no
    /// source (an asset-generation run, or a state that releases nothing).
    pub url: Option<String>,
}

/// `POST /runs/{id}/publish`: create the run's public repository and push the
/// scrubbed bundle to it, answering its URL (`200`, `{ "url" }`).
///
/// The publisher presents its per-publish-job token and names its publish job
/// in `x-tcab-publish-job-id`. The run's record is read from the backend with
/// the service token. A run that releases no source answers `{ "url": null }`
/// and touches nothing. `404` when the backend holds no such run or no bundle
/// is stored, `422` when the stored bundle or the record cannot be published,
/// `409` when the repository's `main` holds a different tree, `503` when the
/// service is not configured to publish, and `502` when an upstream failed,
/// which the publisher retries; every step is safe to repeat.
#[tracing::instrument(name = "repos.publish_route", skip(state, headers), fields(run.id = %id), err(Debug))]
async fn publish(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<axum::Json<PublishOut>, ApiError> {
    let key = bundle_key(&id)?;
    let token =
        bearer(&headers).ok_or_else(|| ApiError::unauthorized("missing publish-job token"))?;
    let job = job_id(&headers, PUBLISH_JOB_ID_HEADER)?;
    verify_publish_job_token(&state.http, &state.backend_url, job, &token).await?;

    let publisher = state.publisher.as_ref().ok_or_else(|| {
        ApiError::unconfigured(
            "this service publishes nothing: `TCAB_GITHUB_ORG` and `TCAB_GITHUB_TOKEN` are unset",
        )
    })?;
    let service_token = state.service_token.as_ref().ok_or_else(|| {
        ApiError::unconfigured(
            "a publish reads the run's record with `TCAB_BACKEND_SERVICE_TOKEN`, which is unset",
        )
    })?;
    let record =
        crate::backend::fetch_record(&state.http, &state.backend_url, service_token, &id).await?;
    if !crate::publish::releases_source(&record) {
        tracing::info!(run.id = %id, "the run releases no source; nothing is published");
        return Ok(axum::Json(PublishOut { url: None }));
    }

    let dir = scratch()?;
    let path = dir.path().join("run.bundle");
    if !state
        .blob
        .download_file(&key, &path)
        .await
        .map_err(store_error)?
    {
        return Err(ApiError::not_found(format!(
            "no bundle is stored for run `{id}`"
        )));
    }
    let url = publisher
        .publish(&record, &path, dir.path())
        .await
        .map_err(publish_error)?;
    Ok(axum::Json(PublishOut { url }))
}

/// A publish failure, by whose failure it is.
fn publish_error(err: PublishError) -> ApiError {
    match err {
        PublishError::Bundle(_) | PublishError::EndTime(_) => {
            ApiError::unprocessable(err.to_string())
        }
        PublishError::Diverged { .. } => ApiError::conflict(err.to_string()),
        PublishError::GitHub(_) | PublishError::Push { .. } => ApiError::upstream(err.to_string()),
        PublishError::Git(_) | PublishError::Io(_) => ApiError::internal(err.to_string()),
    }
}

#[cfg(test)]
#[path = "api.test.rs"]
mod tests;
