//! GitHub: the organization a published run's repository lives in, the API call
//! that creates it, and the credential a push presents.
//!
//! The service holds its own token (`TCAB_GITHUB_TOKEN`) and is the only
//! component that does, so it never shells out to `gh`: the repository is
//! looked up and created over the REST API, and `git` pushes with the token in
//! an `http.extraHeader` handed to it through the environment, never on its
//! command line.

use base64::Engine as _;
use serde::Deserialize;

/// The REST API version every request names.
const API_VERSION: &str = "2022-11-28";
/// The `User-Agent` GitHub requires on every API request.
const USER_AGENT: &str = "tcab-repos";

/// Where and as whom the service publishes.
#[derive(Clone, PartialEq, Eq)]
pub struct GitHubConfig {
    /// The organization a run's repository is created under (`TCAB_GITHUB_ORG`).
    pub org: String,
    /// The token that creates the repository and pushes to it
    /// (`TCAB_GITHUB_TOKEN`).
    pub token: String,
    /// The REST API's base URL, without a trailing slash.
    pub api_url: String,
    /// The base URL a repository is pushed to as `<git_url>/<org>/<name>.git`.
    pub git_url: String,
    /// The base URL a repository is browsed at, which the publish answers.
    pub web_url: String,
}

impl std::fmt::Debug for GitHubConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubConfig")
            .field("org", &self.org)
            .field("token", &"<redacted>")
            .field("api_url", &self.api_url)
            .field("git_url", &self.git_url)
            .field("web_url", &self.web_url)
            .finish()
    }
}

impl GitHubConfig {
    /// The configuration of `org` on github.com, with `token`.
    pub fn new(org: String, token: String) -> Self {
        Self {
            org,
            token,
            api_url: "https://api.github.com".to_string(),
            git_url: "https://github.com".to_string(),
            web_url: "https://github.com".to_string(),
        }
    }

    /// The public URL of repository `name`, which a publish answers.
    pub fn repo_url(&self, name: &str) -> String {
        format!("{}/{}/{name}", self.web_url, self.org)
    }

    /// The URL repository `name` is pushed to and fetched from.
    pub fn remote_url(&self, name: &str) -> String {
        format!("{}/{}/{name}.git", self.git_url, self.org)
    }

    /// The environment that hands `git` the token for a push or a fetch of
    /// the organization's repositories: one `http.<git_url>/.extraHeader`
    /// through `GIT_CONFIG_COUNT`, so the token never appears in a process's
    /// arguments, a remote URL or a configuration file.
    pub fn git_auth_env(&self) -> Vec<(String, String)> {
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("x-access-token:{}", self.token));
        vec![
            ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
            (
                "GIT_CONFIG_KEY_0".to_string(),
                format!("http.{}/.extraHeader", self.git_url),
            ),
            (
                "GIT_CONFIG_VALUE_0".to_string(),
                format!("Authorization: Basic {basic}"),
            ),
        ]
    }
}

/// A GitHub API call failed.
#[derive(Debug, thiserror::Error)]
pub enum GitHubError {
    /// The request could not be sent, or its response not read.
    #[error("GitHub {operation} `{repo}`: {source}")]
    Transport {
        /// The operation (`view`, `create`).
        operation: &'static str,
        /// The repository it named.
        repo: String,
        /// The transport failure.
        source: reqwest::Error,
    },
    /// GitHub answered a status the operation does not accept.
    #[error("GitHub {operation} `{repo}` answered {status}: {detail}")]
    Status {
        /// The operation.
        operation: &'static str,
        /// The repository it named.
        repo: String,
        /// The HTTP status.
        status: u16,
        /// The response body, which carries GitHub's explanation.
        detail: String,
    },
}

/// The one field of a `422` validation error the create looks for.
#[derive(Deserialize)]
struct ValidationErrors {
    #[serde(default)]
    errors: Vec<ValidationError>,
}

/// One entry of a `422`'s `errors`.
#[derive(Deserialize)]
struct ValidationError {
    #[serde(default)]
    message: String,
}

/// Make sure repository `name` exists in the organization, creating it public
/// and empty when it does not. Answers whether this call created it, which is
/// what decides whether the first push waits out GitHub's permission
/// propagation. A create that loses a race with another replica (`422`, the
/// name already exists) reuses the repository the other one made.
#[tracing::instrument(name = "github.ensure_repo", skip(http, github, description), fields(github.org = %github.org), err)]
pub async fn ensure_repo(
    http: &reqwest::Client,
    github: &GitHubConfig,
    name: &str,
    description: &str,
) -> Result<bool, GitHubError> {
    let qualified = format!("{}/{name}", github.org);
    let transport = |operation| {
        let repo = qualified.clone();
        move |source| GitHubError::Transport {
            operation,
            repo,
            source,
        }
    };
    let view = request(
        http,
        github,
        reqwest::Method::GET,
        &format!("/repos/{qualified}"),
    )
    .send()
    .await
    .map_err(transport("view"))?;
    match view.status().as_u16() {
        200 => return Ok(false),
        404 => {}
        status => {
            return Err(GitHubError::Status {
                operation: "view",
                repo: qualified,
                status,
                detail: view.text().await.unwrap_or_default(),
            });
        }
    }
    let create = request(
        http,
        github,
        reqwest::Method::POST,
        &format!("/orgs/{}/repos", github.org),
    )
    .json(&serde_json::json!({
        "name": name,
        "description": description,
        "visibility": "public",
        "private": false,
        "auto_init": false,
    }))
    .send()
    .await
    .map_err(transport("create"))?;
    let status = create.status().as_u16();
    if status == 201 {
        return Ok(true);
    }
    let detail = create.text().await.unwrap_or_default();
    if status == 422 && names_an_existing_repo(&detail) {
        return Ok(false);
    }
    Err(GitHubError::Status {
        operation: "create",
        repo: qualified,
        status,
        detail,
    })
}

/// Whether a `422` body says the name is already taken in the organization.
fn names_an_existing_repo(body: &str) -> bool {
    serde_json::from_str::<ValidationErrors>(body).is_ok_and(|errors| {
        errors
            .errors
            .iter()
            .any(|error| error.message.contains("already exists"))
    })
}

/// An authenticated API request to `path`.
fn request(
    http: &reqwest::Client,
    github: &GitHubConfig,
    method: reqwest::Method,
    path: &str,
) -> reqwest::RequestBuilder {
    http.request(method, format!("{}{path}", github.api_url))
        .bearer_auth(&github.token)
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", API_VERSION)
        .header("user-agent", USER_AGENT)
}

#[cfg(test)]
#[path = "github.test.rs"]
mod tests;
