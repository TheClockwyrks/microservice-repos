//! The repos service: the write-through store of run implementations as git
//! bundles, and the public push of a published run's repository.
//!
//! What the service is required to be is stated by the documentation site of
//! The Test Cabinet superrepo, `components/repos/overview.md`. The pieces:
//!
//! - [`api`]: the routes, `POST`, `GET` and `DELETE /runs/{id}/bundle` and
//!   `POST /runs/{id}/publish`.
//! - [`auth`]: the token each route takes.
//! - [`bundle`]: an upload staged on disk and checked to be a usable bundle.
//! - [`blob`]: the private Blob container the bundles live in, one object per
//!   run, by workload identity in a cluster and Shared Key against Azurite.
//! - [`publish`]: the deterministic, scrubbed commit and the push of `main`.
//! - [`github`]: the organization, the repository create and the push
//!   credential.
//! - [`backend`]: the run record a publish reads.
//! - [`git`]: `git`, isolated from the host's configuration.
//! - [`config`]: the environment-sourced configuration.

pub mod api;
pub mod auth;
pub mod backend;
pub mod blob;
pub mod bundle;
pub mod config;
pub mod error;
pub mod git;
pub mod github;
pub mod publish;

#[cfg(test)]
pub(crate) mod testing;

use std::sync::Arc;

use crate::api::AppState;
use crate::blob::{BlobClient, BlobError};
use crate::config::Config;
use crate::publish::Publisher;

/// A wired, runnable service: the router and the address it binds.
pub struct ReposService {
    /// The Axum router, ready to be served.
    pub router: axum::Router,
    /// The bind address.
    pub bind: String,
}

/// Make the bundle container ready before the service serves: on an emulator,
/// create it (one that exists is left as it is); then, in every mode, list one
/// blob of it, which proves the credential and the container. A cluster whose
/// identity was not provisioned or federated fails here, so its pod never
/// becomes ready rather than failing every request later. Answers whether the
/// container was created.
pub async fn prepare_container(config: &Config) -> Result<bool, BlobError> {
    let blob = BlobClient::new(&config.blob, reqwest::Client::new())?;
    let created = if config.creates_container() {
        blob.create_container().await?
    } else {
        false
    };
    blob.probe().await?;
    Ok(created)
}

/// Assemble the service from a configuration. Fails when the container's
/// credential cannot be set up (a workload identity the pod was not given).
pub fn build(config: Config) -> Result<ReposService, BlobError> {
    let http = reqwest::Client::new();
    let blob = BlobClient::new(&config.blob, http.clone())?;
    let publisher = config
        .github
        .map(|github| Arc::new(Publisher::new(github, http.clone())));
    let state = AppState {
        blob,
        http,
        backend_url: Arc::new(config.backend_url),
        service_token: config.service_token.map(Arc::new),
        publisher,
        upload_limit: bundle::MAX_BUNDLE_BYTES,
    };
    Ok(ReposService {
        router: api::router(state),
        bind: config.bind,
    })
}
