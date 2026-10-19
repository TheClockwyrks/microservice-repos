//! Who may call each route.
//!
//! The service sits behind no edge path and has no Kubernetes API access: every
//! caller is in the cluster and presents a bearer token in
//! `Authorization: Bearer <token>`, and each route checks the one token it
//! takes (see the service's Auth section in the documentation site):
//!
//! - The upload takes the driver's per-job token, forwarded to the backend's
//!   `POST /jobs/{id}/verify-token` with the job id the driver sends in
//!   `x-tcab-job-id` ([`verify_job_token`]), exactly as an artifacts upload is.
//! - The publish takes the per-publish-job token, forwarded to
//!   `POST /publish-jobs/{id}/verify-token` with the publish job id in
//!   `x-tcab-publish-job-id` ([`verify_publish_job_token`]).
//! - The download and the delete take the shared control-plane service token
//!   (`TCAB_BACKEND_SERVICE_TOKEN`), compared in constant time
//!   ([`verify_service_token`]).
//!
//! The backend is the authority for both job tokens: a `401` or `404` from it is
//! a rejected token (`401` here), and anything else an unavailable authority
//! (`502`), which the caller retries.

use axum::http::HeaderMap;

use crate::error::ApiError;

/// The header a driver names its job in on an upload. The path names the run,
/// a different id from the job the token was minted for.
pub const JOB_ID_HEADER: &str = "x-tcab-job-id";

/// The header the publisher names its publish job in on a publish.
pub const PUBLISH_JOB_ID_HEADER: &str = "x-tcab-publish-job-id";

/// Verify an upload's per-job token for job `job_id` against the backend.
pub async fn verify_job_token(
    http: &reqwest::Client,
    backend_url: &str,
    job_id: &str,
    token: &str,
) -> Result<(), ApiError> {
    verify_with_backend(
        http,
        &format!("{backend_url}/jobs/{job_id}/verify-token"),
        token,
        "job",
    )
    .await
}

/// Verify a publish's per-publish-job token for publish job `job_id` against
/// the backend.
pub async fn verify_publish_job_token(
    http: &reqwest::Client,
    backend_url: &str,
    job_id: &str,
    token: &str,
) -> Result<(), ApiError> {
    verify_with_backend(
        http,
        &format!("{backend_url}/publish-jobs/{job_id}/verify-token"),
        token,
        "publish-job",
    )
    .await
}

/// Forward `token` to the backend's verify endpoint at `url`.
async fn verify_with_backend(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    kind: &str,
) -> Result<(), ApiError> {
    if token.is_empty() {
        return Err(ApiError::unauthorized(format!("missing {kind} token")));
    }
    let mut headers = http::HeaderMap::new();
    test_cabinet_telemetry::propagation::inject_current_context(&mut headers);
    let response = http
        .post(url)
        .headers(headers)
        .json(&serde_json::json!({ "token": token }))
        .send()
        .await
        .map_err(|err| {
            ApiError::auth_unavailable(format!(
                "verifying the {kind} token with the backend: {err}"
            ))
        })?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else if status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::NOT_FOUND
    {
        Err(ApiError::unauthorized(format!("invalid {kind} token")))
    } else {
        Err(ApiError::auth_unavailable(format!(
            "the backend's {kind}-token verify answered HTTP {status}"
        )))
    }
}

/// Authorize a route of the shared service token. `expected` is `None` when
/// the service has none configured, and every caller is then refused.
pub fn verify_service_token(headers: &HeaderMap, expected: Option<&str>) -> Result<(), ApiError> {
    let expected = expected.ok_or_else(|| {
        ApiError::unauthorized("the service token is not configured on this service")
    })?;
    let presented =
        bearer(headers).ok_or_else(|| ApiError::unauthorized("missing service token"))?;
    if token_matches(&presented, expected) {
        Ok(())
    } else {
        Err(ApiError::unauthorized("invalid service token"))
    }
}

/// The job id a job-token route names in `header`, required and non-empty.
pub fn job_id<'a>(headers: &'a HeaderMap, header: &str) -> Result<&'a str, ApiError> {
    headers
        .get(header)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::unauthorized(format!("missing `{header}` header")))
}

/// Constant-time string equality, so a comparison leaks nothing about the
/// expected token through its timing. A length mismatch is an immediate
/// non-match.
fn token_matches(presented: &str, expected: &str) -> bool {
    let (presented, expected) = (presented.as_bytes(), expected.as_bytes());
    if presented.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in presented.iter().zip(expected.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// The token of an `Authorization: Bearer <token>` header, trimmed, or `None`
/// when the header is absent or carries no bearer token.
pub fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?
        .trim();
    (!token.is_empty()).then(|| token.to_string())
}

#[cfg(test)]
#[path = "auth.test.rs"]
mod tests;
