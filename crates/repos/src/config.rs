//! Repos-service configuration, sourced entirely from environment variables, so
//! the service runs from a container or a systemd unit with no config file.
//!
//! | Variable | Required | Purpose | Default |
//! | --- | --- | --- | --- |
//! | `TCAB_REPOS_BIND` | no | Address the Axum server binds. | `0.0.0.0:8792` |
//! | `TCAB_BACKEND_URL` | no | The backend a driver's per-job token and a publisher's per-publish-job token are forwarded to for verification, and a publish reads the run's record from. | `http://127.0.0.1:8787` |
//! | `TCAB_BACKEND_SERVICE_TOKEN` | no | The shared control-plane service token. It is the only credential of the bundle download and delete, and the credential a publish reads the run's record from the backend with. Unset ⇒ the download and the delete refuse every caller and a publish answers `503`. | _none_ |
//! | `TCAB_REPOS_BLOB_ACCOUNT` | yes | The storage account holding the private bundle container (`devstoreaccount1` against Azurite). | _none_ |
//! | `TCAB_REPOS_BLOB_CONTAINER` | yes | The private container the bundles are written to, one object per run. | _none_ |
//! | `TCAB_REPOS_BLOB_ENDPOINT` | no | The account's Blob endpoint, for an emulator: Azurite's path-style `http://<host>:10000/devstoreaccount1`. Set together with `TCAB_REPOS_BLOB_KEY`. | `https://<account>.blob.core.windows.net` |
//! | `TCAB_REPOS_BLOB_KEY` | no | The account key (base64) requests are signed with by Shared Key, Azurite's well-known key locally. Set together with `TCAB_REPOS_BLOB_ENDPOINT`; unset, requests authenticate by workload identity. | _none_ |
//! | `TCAB_GITHUB_ORG` | no | The GitHub organization a published run's repository is created under: `TheClockwyrks` in prod, `TheClockwyrks-Staging` in staging. Set together with `TCAB_GITHUB_TOKEN`. | _none_ |
//! | `TCAB_GITHUB_TOKEN` | no | The service's own GitHub token, which creates the repository and pushes to it. No other component holds it. Set together with `TCAB_GITHUB_ORG`; both unset ⇒ a publish answers `503`, the local stack's shape. | _none_ |
//!
//! The bundles live in a private Azure Blob container. In a cluster the service
//! authenticates to it by workload identity: the pod's `ServiceAccount` is
//! federated to the service's managed identity, and the webhook projects
//! `AZURE_CLIENT_ID`, `AZURE_TENANT_ID` and `AZURE_FEDERATED_TOKEN_FILE` (see
//! [`crate::blob::WorkloadIdentity`]). Against Azurite it signs with Shared Key
//! instead, which is what `TCAB_REPOS_BLOB_ENDPOINT` and `TCAB_REPOS_BLOB_KEY`
//! select; the two are set together or not at all. In Shared Key mode the
//! service creates its container at startup, so an emulator needs no
//! provisioning; a cluster's container exists with the account.
//!
//! A bind on `0.0.0.0` is deliberate: the service is a data-plane peer the
//! driver, the backend and the publisher reach over the cluster network.

use crate::blob::{BlobAuth, BlobConfig};
use crate::github::GitHubConfig;

/// The default address the Axum server binds when `TCAB_REPOS_BIND` is unset.
const DEFAULT_BIND: &str = "0.0.0.0:8792";
/// The default backend URL when `TCAB_BACKEND_URL` is unset: the backend's own
/// loopback default, so a local run works with both services up and nothing
/// configured.
const DEFAULT_BACKEND_URL: &str = "http://127.0.0.1:8787";

/// The fully resolved repos-service configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address the Axum server binds (`TCAB_REPOS_BIND`).
    pub bind: String,
    /// The backend base URL (`TCAB_BACKEND_URL`), without a trailing slash.
    pub backend_url: String,
    /// The shared control-plane service token (`TCAB_BACKEND_SERVICE_TOKEN`).
    pub service_token: Option<String>,
    /// The private bundle container and how the service authenticates to it
    /// (`TCAB_REPOS_BLOB_*`).
    pub blob: BlobConfig,
    /// The organization and token a publish creates and pushes with
    /// (`TCAB_GITHUB_ORG`, `TCAB_GITHUB_TOKEN`), or `None` when the service
    /// publishes nothing.
    pub github: Option<GitHubConfig>,
}

/// A configuration the service refuses to start with.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// One of a pair of variables that are set together was set alone.
    #[error("`{set}` is set but `{unset}` is not: {hint}")]
    Unpaired {
        /// The variable that was set.
        set: &'static str,
        /// The variable that was not.
        unset: &'static str,
        /// What setting both, or neither, means.
        hint: &'static str,
    },
    /// A required variable is unset.
    #[error("`{0}` is required")]
    Missing(&'static str),
    /// `TCAB_REPOS_BLOB_KEY` is not base64.
    #[error("`TCAB_REPOS_BLOB_KEY` is not a base64 account key")]
    InvalidBlobKey,
}

impl Config {
    /// Resolve the configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Resolve the configuration from `lookup`, which answers a variable's value
    /// or `None` when it is unset. An empty value counts as unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let var = |key: &str| {
            lookup(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        Ok(Self {
            bind: var("TCAB_REPOS_BIND").unwrap_or_else(|| DEFAULT_BIND.to_string()),
            backend_url: var("TCAB_BACKEND_URL")
                .unwrap_or_else(|| DEFAULT_BACKEND_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            service_token: var("TCAB_BACKEND_SERVICE_TOKEN"),
            blob: blob_config(&var)?,
            github: github_config(&var)?,
        })
    }

    /// Whether the service creates its container at startup: on an emulator
    /// (Shared Key) only. Under workload identity the container is provisioned
    /// with the account, and the identity holds no right to create it.
    pub fn creates_container(&self) -> bool {
        matches!(self.blob.auth, BlobAuth::SharedKey(_))
    }
}

/// Resolve the bundle container and its authentication. The endpoint and key
/// together select Shared Key (Azurite); neither selects workload identity
/// against the account's public-cloud endpoint.
fn blob_config(var: &impl Fn(&str) -> Option<String>) -> Result<BlobConfig, ConfigError> {
    use base64::Engine as _;

    let account =
        var("TCAB_REPOS_BLOB_ACCOUNT").ok_or(ConfigError::Missing("TCAB_REPOS_BLOB_ACCOUNT"))?;
    let container = var("TCAB_REPOS_BLOB_CONTAINER")
        .ok_or(ConfigError::Missing("TCAB_REPOS_BLOB_CONTAINER"))?;
    const HINT: &str = "set both to sign with Shared Key against an emulator, or neither to \
                        authenticate by workload identity";
    let (endpoint, auth) = match (var("TCAB_REPOS_BLOB_ENDPOINT"), var("TCAB_REPOS_BLOB_KEY")) {
        (Some(endpoint), Some(key)) => {
            let key = base64::engine::general_purpose::STANDARD
                .decode(key)
                .map_err(|_| ConfigError::InvalidBlobKey)?;
            (
                endpoint.trim_end_matches('/').to_string(),
                BlobAuth::SharedKey(key),
            )
        }
        (None, None) => (
            BlobConfig::default_endpoint(&account),
            BlobAuth::WorkloadIdentity,
        ),
        (Some(_), None) => {
            return Err(ConfigError::Unpaired {
                set: "TCAB_REPOS_BLOB_ENDPOINT",
                unset: "TCAB_REPOS_BLOB_KEY",
                hint: HINT,
            });
        }
        (None, Some(_)) => {
            return Err(ConfigError::Unpaired {
                set: "TCAB_REPOS_BLOB_KEY",
                unset: "TCAB_REPOS_BLOB_ENDPOINT",
                hint: HINT,
            });
        }
    };
    Ok(BlobConfig {
        account,
        container,
        endpoint,
        auth,
    })
}

/// Resolve the GitHub organization and token, set together or not at all.
fn github_config(
    var: &impl Fn(&str) -> Option<String>,
) -> Result<Option<GitHubConfig>, ConfigError> {
    const HINT: &str = "set both to publish runs to GitHub, or neither to refuse every publish";
    match (var("TCAB_GITHUB_ORG"), var("TCAB_GITHUB_TOKEN")) {
        (Some(org), Some(token)) => Ok(Some(GitHubConfig::new(org, token))),
        (None, None) => Ok(None),
        (Some(_), None) => Err(ConfigError::Unpaired {
            set: "TCAB_GITHUB_ORG",
            unset: "TCAB_GITHUB_TOKEN",
            hint: HINT,
        }),
        (None, Some(_)) => Err(ConfigError::Unpaired {
            set: "TCAB_GITHUB_TOKEN",
            unset: "TCAB_GITHUB_ORG",
            hint: HINT,
        }),
    }
}

#[cfg(test)]
#[path = "config.test.rs"]
mod tests;
