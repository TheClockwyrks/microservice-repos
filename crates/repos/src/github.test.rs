//! The repository lookup and create against the stub GitHub API.

use super::*;
use crate::testing::{ORG, serve_github};

#[tokio::test]
async fn a_missing_repository_is_created_and_an_existing_one_reused() {
    let root = tempfile::tempdir().unwrap();
    let (github, stub) = serve_github(root.path()).await;
    let http = reqwest::Client::new();
    assert!(ensure_repo(&http, &github, "tcab-a", "d").await.unwrap());
    assert!(!ensure_repo(&http, &github, "tcab-a", "d").await.unwrap());
    assert_eq!(
        stub.lock().unwrap().calls,
        [
            format!("GET /repos/{ORG}/tcab-a"),
            format!("POST /orgs/{ORG}/repos"),
            format!("GET /repos/{ORG}/tcab-a"),
        ]
    );
}

#[tokio::test]
async fn a_create_that_loses_a_race_reuses_the_repository() {
    let root = tempfile::tempdir().unwrap();
    let (github, stub) = serve_github(root.path()).await;
    stub.lock().unwrap().create_races = true;
    let http = reqwest::Client::new();
    assert!(!ensure_repo(&http, &github, "tcab-a", "d").await.unwrap());
}

#[tokio::test]
async fn a_refused_token_is_an_error() {
    let root = tempfile::tempdir().unwrap();
    let (mut github, _stub) = serve_github(root.path()).await;
    github.token = "wrong".to_string();
    let err = ensure_repo(&reqwest::Client::new(), &github, "tcab-a", "d")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            GitHubError::Status {
                status: 401,
                operation: "view",
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn the_push_credential_is_an_extra_header_for_the_git_host() {
    let github = GitHubConfig::new("TheClockwyrks".to_string(), "t0k".to_string());
    let env = github.git_auth_env();
    assert_eq!(env[0], ("GIT_CONFIG_COUNT".to_string(), "1".to_string()));
    assert_eq!(env[1].1, "http.https://github.com/.extraHeader");
    // base64("x-access-token:t0k")
    assert_eq!(env[2].1, "Authorization: Basic eC1hY2Nlc3MtdG9rZW46dDBr");
    assert_eq!(
        github.remote_url("tcab-a"),
        "https://github.com/TheClockwyrks/tcab-a.git"
    );
    assert_eq!(
        github.repo_url("tcab-a"),
        "https://github.com/TheClockwyrks/tcab-a"
    );
}

#[test]
fn only_a_name_already_taken_counts_as_existing() {
    assert!(names_an_existing_repo(
        r#"{"errors":[{"message":"name already exists on this account"}]}"#
    ));
    assert!(!names_an_existing_repo(
        r#"{"errors":[{"message":"name is too long"}]}"#
    ));
    assert!(!names_an_existing_repo("not json"));
}
