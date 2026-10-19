//! Staging an upload and checking it is a usable bundle.

use axum::body::Body;

use super::*;
use crate::git::Git;
use crate::testing::make_bundle;

#[tokio::test]
async fn a_body_is_staged_under_its_cap_and_refused_past_it() {
    let dir = tempfile::tempdir().unwrap();
    let (path, length) = stage(Body::from("0123456789"), 10, dir.path())
        .await
        .unwrap();
    assert_eq!(length, 10);
    assert_eq!(std::fs::read(path).unwrap(), b"0123456789");
    let err = stage(Body::from("0123456789x"), 10, dir.path())
        .await
        .unwrap_err();
    assert!(matches!(err, BundleError::TooLarge { limit: 10 }), "{err}");
}

#[tokio::test]
async fn a_bundle_of_the_whole_repository_passes() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = make_bundle(dir.path(), &[("src/main.js", b"play()\n")]).await;
    let scratch = tempfile::tempdir().unwrap();
    check(&bundle, scratch.path()).await.unwrap();
}

#[tokio::test]
async fn a_file_that_is_not_a_bundle_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.bundle");
    std::fs::write(&path, b"not a bundle").unwrap();
    let err = check(&path, dir.path()).await.unwrap_err();
    assert!(matches!(err, BundleError::Invalid(_)), "{err}");
}

#[tokio::test]
async fn a_bundle_without_main_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    make_bundle(dir.path(), &[]).await;
    let work = dir.path().join("work");
    let git = Git::new(&work, dir.path());
    git.run(&["branch", "--quiet", "--move", "main", "trunk"])
        .await
        .unwrap();
    let path = dir.path().join("trunk.bundle");
    git.run(&[
        "bundle",
        "create",
        "--quiet",
        &path.display().to_string(),
        "--all",
    ])
    .await
    .unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let err = check(&path, scratch.path()).await.unwrap_err();
    assert!(
        matches!(&err, BundleError::Invalid(message) if message.contains("refs/heads/main")),
        "{err}"
    );
}

#[tokio::test]
async fn a_bundle_that_needs_commits_it_lacks_is_invalid() {
    let dir = tempfile::tempdir().unwrap();
    make_bundle(dir.path(), &[("a.txt", b"a\n")]).await;
    let work = dir.path().join("work");
    let git = Git::new(&work, dir.path());
    // Only the last commit, with the seed as a prerequisite.
    let path = dir.path().join("thin.bundle");
    git.run(&[
        "bundle",
        "create",
        "--quiet",
        &path.display().to_string(),
        "main~1..main",
    ])
    .await
    .unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let err = check(&path, scratch.path()).await.unwrap_err();
    assert!(matches!(err, BundleError::Invalid(_)), "{err}");
}
