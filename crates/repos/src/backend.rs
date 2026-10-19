//! The one read the service makes of the backend: a run's record, which a
//! publish takes its slug, its end time and its publishability from.

use serde::Deserialize;
use test_cabinet_contracts::run_record::RunRecord;

use crate::error::ApiError;

/// The part of `GET /runs/{id}` a publish reads.
#[derive(Deserialize)]
struct StoredRun {
    record: RunRecord,
}

/// Read run `id`'s record from the backend at `backend_url`, presenting the
/// service token. An unknown run is a `404`; an unreachable or failing backend
/// a `502`, which the publisher retries.
pub async fn fetch_record(
    http: &reqwest::Client,
    backend_url: &str,
    service_token: &str,
    id: &str,
) -> Result<RunRecord, ApiError> {
    let mut headers = http::HeaderMap::new();
    test_cabinet_telemetry::propagation::inject_current_context(&mut headers);
    let response = http
        .get(format!("{backend_url}/runs/{id}"))
        .headers(headers)
        .bearer_auth(service_token)
        .send()
        .await
        .map_err(|err| ApiError::upstream(format!("reading run `{id}` from the backend: {err}")))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(ApiError::not_found(format!(
            "the backend holds no run `{id}`"
        )));
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(ApiError::upstream(format!(
            "reading run `{id}` from the backend answered HTTP {status}: {}",
            body.trim()
        )));
    }
    let run: StoredRun = response.json().await.map_err(|err| {
        ApiError::upstream(format!(
            "reading run `{id}`'s record from the backend: {err}"
        ))
    })?;
    Ok(run.record)
}
