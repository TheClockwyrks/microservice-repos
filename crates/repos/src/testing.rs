//! Stubs the tests share: the backend's token verifies and run read, GitHub's
//! repository API over bare repositories on disk, and a run's bundle made with
//! git as the driver makes it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use test_cabinet_contracts::run_record::RunRecord;

use crate::git::Git;
use crate::github::GitHubConfig;

/// The job the stub backend minted [`JOB_TOKEN`] for.
pub const JOB_ID: &str = "job-1";
/// The driver's per-job token.
pub const JOB_TOKEN: &str = "job-token";
/// The publish job the stub backend minted [`PUBLISH_TOKEN`] for.
pub const PUBLISH_JOB_ID: &str = "publish-1";
/// The publisher's per-publish-job token.
pub const PUBLISH_TOKEN: &str = "publish-token";
/// A job id the stub backend answers `500` for.
pub const BROKEN_JOB_ID: &str = "broken";
/// The shared service token.
pub const SERVICE_TOKEN: &str = "service-token";
/// The organization the stub GitHub holds.
pub const ORG: &str = "TheClockwyrks-Staging";

/// A run record of a completed end-to-end run with id `id`.
pub fn record(id: &str) -> RunRecord {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "startedAt": "2026-10-19T10:00:00Z",
        "finishedAt": "2026-10-19T10:42:17Z",
        "subject": {
            "testCaseSlug": "pong",
            "testCaseVersion": "v1.0.0",
            "testType": "end-to-end",
            "variant": "base",
            "harnessSlug": "codex",
            "orchestratorSlug": "one-shot",
            "engineSlug": "none",
            "modelId": "openai/gpt-5.4-mini"
        },
        "tooling": {},
        "environment": {
            "os": "Debian GNU/Linux 12 (bookworm)",
            "containerImage": "test-cabinet/codex:latest",
            "authMode": "apiKey"
        },
        "metrics": { "runTimeSeconds": 2537.0, "tokens": {}, "cost": {} },
        "validation": { "loaded": true, "detail": null, "checks": [] },
        "links": { "sourceRepo": null, "playableBuild": null },
        "status": { "state": "completed" }
    }))
    .expect("the stub record matches the contract")
}

/// The stub backend's state.
#[derive(Default)]
pub struct Backend {
    /// The records `GET /runs/{id}` answers.
    pub runs: BTreeMap<String, RunRecord>,
    /// Every verify call, as `<kind>:<job id>`.
    pub verifies: Vec<String>,
}

/// The stub backend's state, shared with the test.
pub type SharedBackend = Arc<Mutex<Backend>>;

#[derive(serde::Deserialize)]
struct TokenIn {
    token: String,
}

async fn verify(
    stub: &SharedBackend,
    kind: &str,
    id: &str,
    token: &str,
    expected: (&str, &str),
) -> StatusCode {
    stub.lock().unwrap().verifies.push(format!("{kind}:{id}"));
    if id == BROKEN_JOB_ID {
        StatusCode::INTERNAL_SERVER_ERROR
    } else if id != expected.0 {
        StatusCode::NOT_FOUND
    } else if token != expected.1 {
        StatusCode::UNAUTHORIZED
    } else {
        StatusCode::NO_CONTENT
    }
}

async fn verify_job(
    State(stub): State<SharedBackend>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<TokenIn>,
) -> StatusCode {
    verify(&stub, "job", &id, &body.token, (JOB_ID, JOB_TOKEN)).await
}

async fn verify_publish_job(
    State(stub): State<SharedBackend>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<TokenIn>,
) -> StatusCode {
    verify(
        &stub,
        "publish-job",
        &id,
        &body.token,
        (PUBLISH_JOB_ID, PUBLISH_TOKEN),
    )
    .await
}

async fn run(
    State(stub): State<SharedBackend>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let authorized = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(&format!("Bearer {SERVICE_TOKEN}"));
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match stub.lock().unwrap().runs.get(&id) {
        Some(record) => Json(serde_json::json!({
            "record": record,
            "reviews": [],
            "links": { "sourceRepo": null, "playableBuild": null },
            "published": false,
        }))
        .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Serve the stub backend on a loopback port, answering its URL and state.
pub async fn serve_backend() -> (String, SharedBackend) {
    let stub = SharedBackend::default();
    let app = axum::Router::new()
        .route("/jobs/{id}/verify-token", post(verify_job))
        .route("/publish-jobs/{id}/verify-token", post(verify_publish_job))
        .route("/runs/{id}", get(run))
        .with_state(Arc::clone(&stub));
    (serve(app).await, stub)
}

/// The stub GitHub's state: the repositories of [`ORG`], each a bare repository
/// under `root`.
pub struct GitHub {
    /// Where the bare repositories live, as `<root>/<org>/<name>.git`.
    pub root: PathBuf,
    /// The names of the repositories that exist.
    pub repos: BTreeSet<String>,
    /// Every API call, as `<METHOD> <path>`.
    pub calls: Vec<String>,
    /// How many pushes to a repository created from now on its remote refuses
    /// before accepting one, standing in for GitHub's permission propagation.
    pub refused_pushes: u32,
    /// Whether a create answers `422`, the name already taken, as a replica
    /// losing a race does.
    pub create_races: bool,
}

/// The stub GitHub's state, shared with the test.
pub type SharedGitHub = Arc<Mutex<GitHub>>;

/// Create the bare repository `name` under `root`, whose `pre-receive` hook
/// refuses the first `refused` pushes.
pub fn init_remote(root: &Path, name: &str, refused: u32) {
    let dir = root.join(ORG).join(format!("{name}.git"));
    std::fs::create_dir_all(&dir).unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "--quiet", "--bare", "--initial-branch", "main"])
        .current_dir(&dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success());
    if refused > 0 {
        let hook = dir.join("hooks").join("pre-receive");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\ncount=$(cat refused 2>/dev/null || echo 0)\n\
                 if [ \"$count\" -lt {refused} ]; then echo $((count + 1)) > refused; \
                 echo 'Permission denied' >&2; exit 1; fi\n"
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

async fn view_repo(
    State(stub): State<SharedGitHub>,
    UrlPath((org, name)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let mut stub = stub.lock().unwrap();
    stub.calls.push(format!("GET /repos/{org}/{name}"));
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if org == ORG && stub.repos.contains(&name) {
        Json(serde_json::json!({ "name": name })).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn create_repo(
    State(stub): State<SharedGitHub>,
    UrlPath(org): UrlPath<String>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let mut stub = stub.lock().unwrap();
    stub.calls.push(format!("POST /orgs/{org}/repos"));
    if !authorized(&headers) || org != ORG {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    assert_eq!(body["visibility"], "public");
    let name = body["name"].as_str().unwrap().to_string();
    if stub.create_races || stub.repos.contains(&name) {
        if !stub.repos.contains(&name) {
            init_remote(&stub.root, &name, 0);
            stub.repos.insert(name);
        }
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "message": "Repository creation failed.",
                "errors": [{ "resource": "Repository", "code": "custom", "field": "name",
                             "message": "name already exists on this account" }],
            })),
        )
            .into_response();
    }
    init_remote(&stub.root, &name, stub.refused_pushes);
    stub.repos.insert(name.clone());
    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "name": name })),
    )
        .into_response()
}

fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some("Bearer gh-token")
        && headers.contains_key("user-agent")
}

/// Serve the stub GitHub API on a loopback port, with its repositories under
/// `root`, answering the configuration that publishes to it and its state.
pub async fn serve_github(root: &Path) -> (GitHubConfig, SharedGitHub) {
    let stub = Arc::new(Mutex::new(GitHub {
        root: root.to_path_buf(),
        repos: BTreeSet::new(),
        calls: Vec::new(),
        refused_pushes: 0,
        create_races: false,
    }));
    let app = axum::Router::new()
        .route("/repos/{org}/{name}", get(view_repo))
        .route("/orgs/{org}/repos", post(create_repo))
        .with_state(Arc::clone(&stub));
    let api_url = serve(app).await;
    let config = GitHubConfig {
        org: ORG.to_string(),
        token: "gh-token".to_string(),
        api_url,
        git_url: format!("file://{}", root.display()),
        web_url: "https://github.com".to_string(),
    };
    (config, stub)
}

/// Serve `app` on a loopback port, answering its origin.
async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{address}")
}

/// A run's repository as the driver leaves it at run end, under `dir`: the seed
/// commit, then the model's `files` committed on top, written as a bundle with
/// `git bundle create <file> --all`. Answers the bundle's path.
pub async fn make_bundle(dir: &Path, files: &[(&str, &[u8])]) -> PathBuf {
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
    std::fs::write(work.join(".gitignore"), "node_modules/\n").unwrap();
    git.run(&["add", "--all"]).await.unwrap();
    git.run(&["commit", "--quiet", "--message", "Seed test case"])
        .await
        .unwrap();
    for (path, contents) in files {
        let path = work.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    git.run(&["add", "--all"]).await.unwrap();
    git.run(&[
        "commit",
        "--quiet",
        "--allow-empty",
        "--message",
        "Implement test case",
    ])
    .await
    .unwrap();
    let bundle = dir.join("run.bundle");
    git.run(&[
        "bundle",
        "create",
        "--quiet",
        &bundle.display().to_string(),
        "--all",
    ])
    .await
    .unwrap();
    bundle
}
