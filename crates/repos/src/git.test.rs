//! The isolated git runner.

use super::*;

#[tokio::test]
async fn a_command_runs_in_its_directory_and_reads_no_host_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let git = Git::new(dir.path(), dir.path());
    git.run(&["init", "--quiet", "--bare"]).await.unwrap();
    assert_eq!(
        git.run(&["rev-parse", "--is-bare-repository"])
            .await
            .unwrap()
            .text(),
        "true"
    );
    // No user is configured anywhere git looks.
    let user = git.run(&["config", "user.name"]).await;
    assert!(matches!(user, Err(GitError::Failed { .. })), "{user:?}");
}

#[tokio::test]
async fn input_is_written_while_output_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let git = Git::new(dir.path(), dir.path());
    git.run(&["init", "--quiet", "--bare"]).await.unwrap();
    // Larger than a pipe's buffer in both directions.
    let content = vec![b'x'; 1024 * 1024];
    let sha = git
        .run_with_input(&["hash-object", "-w", "--stdin"], Some(content.clone()))
        .await
        .unwrap()
        .text();
    let out = git
        .run_with_input(
            &["cat-file", "--batch"],
            Some(format!("{sha}\n").into_bytes()),
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout.len(),
        sha.len() + " blob 1048576\n".len() + content.len() + 1
    );
}

#[tokio::test]
async fn a_failure_carries_its_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let err = Git::new(dir.path(), dir.path())
        .run(&["rev-parse", "--verify", "nothing"])
        .await
        .unwrap_err();
    match err {
        GitError::Failed { args, stderr } => {
            assert_eq!(args, "rev-parse --verify nothing");
            assert!(!stderr.is_empty());
        }
        other => panic!("{other}"),
    }
}
