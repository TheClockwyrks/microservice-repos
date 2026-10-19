//! The publish against the stub GitHub API and bare repositories on disk: the
//! repository, the deterministic commit, the scrub, and every repeat.

use std::time::Duration;

use super::*;
use crate::testing::{ORG, make_bundle, record, serve_github};

/// A key-shaped token, built at run time so no key appears in the source.
fn leaked_key() -> String {
    format!("sk-{}", "a1B2c3D4".repeat(5))
}

fn no_wait() -> PushRetry {
    PushRetry {
        attempts: 3,
        initial_delay: Duration::ZERO,
        backoff: Duration::from_millis(1),
    }
}

/// `git` over the bare remote of repository `name` under `root`.
fn remote(root: &Path, name: &str) -> Git {
    let dir = root.join(ORG).join(format!("{name}.git"));
    Git::new(&dir, root)
}

#[tokio::test]
async fn a_publish_creates_the_repository_and_pushes_a_deterministic_commit() {
    let work = tempfile::tempdir().unwrap();
    let bundle = make_bundle(work.path(), &[("src/main.js", b"play();\n")]).await;
    let root = tempfile::tempdir().unwrap();
    let (github, stub) = serve_github(root.path()).await;
    let publisher = Publisher::new(github, reqwest::Client::new()).with_retry(no_wait());
    let run = record("d483a2f9-1111-2222-3333-444455556666");

    let scratch = tempfile::tempdir().unwrap();
    let url = publisher
        .publish(&run, &bundle, scratch.path())
        .await
        .unwrap();
    let name = "tcab-pong-codex-openai-gpt-5-4-mini-d483a2f9";
    assert_eq!(
        url.as_deref(),
        Some(format!("https://github.com/{ORG}/{name}").as_str())
    );
    assert_eq!(
        stub.lock().unwrap().calls,
        [
            format!("GET /repos/{ORG}/{name}"),
            format!("POST /orgs/{ORG}/repos"),
        ]
    );

    let remote = remote(root.path(), name);
    let log = remote
        .run(&["log", "--format=%an <%ae>|%cn <%ce>|%at|%ct|%s", "main"])
        .await
        .unwrap()
        .text();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 3, "{log}");
    let finished = commit_date("2026-10-19T10:42:17Z").unwrap();
    let seconds = finished.split(' ').next().unwrap();
    assert_eq!(
        lines[0],
        format!(
            "Clockwyrks <runs@clockwyrks.invalid>|Clockwyrks <runs@clockwyrks.invalid>|{seconds}|{seconds}|{PUBLISH_COMMIT_MESSAGE}"
        )
    );
    assert!(lines[1].ends_with("|Implement test case"), "{log}");
    assert!(lines[2].ends_with("|Seed test case"), "{log}");

    // With nothing to redact, the bundle's own commits are pushed as they are.
    let bundle_tip = Git::new(&work.path().join("work"), work.path())
        .run(&["rev-parse", "main"])
        .await
        .unwrap()
        .text();
    assert_eq!(
        remote.run(&["rev-parse", "main~1"]).await.unwrap().text(),
        bundle_tip
    );
    assert_eq!(
        remote
            .run(&["show", "main:src/main.js"])
            .await
            .unwrap()
            .text(),
        "play();"
    );
}

#[tokio::test]
async fn a_repeat_reproduces_the_same_sha_and_succeeds() {
    let work = tempfile::tempdir().unwrap();
    let bundle = make_bundle(work.path(), &[("src/main.js", b"play();\n")]).await;
    let root = tempfile::tempdir().unwrap();
    let (github, stub) = serve_github(root.path()).await;
    let publisher = Publisher::new(github, reqwest::Client::new()).with_retry(no_wait());
    let run = record("d483a2f9-1111-2222-3333-444455556666");

    let first_scratch = tempfile::tempdir().unwrap();
    let first = prepare(&run, &bundle, first_scratch.path()).await.unwrap();
    let second_scratch = tempfile::tempdir().unwrap();
    let second = prepare(&run, &bundle, second_scratch.path()).await.unwrap();
    assert_eq!(first, second);

    let one = tempfile::tempdir().unwrap();
    let url = publisher.publish(&run, &bundle, one.path()).await.unwrap();
    let two = tempfile::tempdir().unwrap();
    assert_eq!(
        publisher.publish(&run, &bundle, two.path()).await.unwrap(),
        url
    );

    let name = repo_name(&run);
    assert_eq!(
        remote(root.path(), &name)
            .run(&["rev-parse", "main"])
            .await
            .unwrap()
            .text(),
        first.commit
    );
    // The repeat reused the repository.
    let calls = stub.lock().unwrap().calls.clone();
    assert_eq!(
        calls.iter().filter(|call| call.starts_with("POST")).count(),
        1
    );
}

#[tokio::test]
async fn a_remote_holding_the_same_tree_under_another_commit_is_a_success() {
    let work = tempfile::tempdir().unwrap();
    let bundle = make_bundle(work.path(), &[("src/main.js", b"play();\n")]).await;
    let root = tempfile::tempdir().unwrap();
    let (github, stub) = serve_github(root.path()).await;
    let publisher = Publisher::new(github, reqwest::Client::new()).with_retry(no_wait());
    let run = record("d483a2f9-1111-2222-3333-444455556666");
    let name = repo_name(&run);

    // An earlier publish, dated differently, landed the same tree.
    let mut earlier = run.clone();
    earlier.finished_at = "2026-10-18T09:00:00Z".to_string();
    let scratch = tempfile::tempdir().unwrap();
    let landed = publisher
        .publish(&earlier, &bundle, scratch.path())
        .await
        .unwrap();
    let landed_sha = remote(root.path(), &name)
        .run(&["rev-parse", "main"])
        .await
        .unwrap()
        .text();

    let scratch = tempfile::tempdir().unwrap();
    assert_eq!(
        publisher
            .publish(&run, &bundle, scratch.path())
            .await
            .unwrap(),
        landed
    );
    // The remote is left as it was: a plain push never rewrites it.
    assert_eq!(
        remote(root.path(), &name)
            .run(&["rev-parse", "main"])
            .await
            .unwrap()
            .text(),
        landed_sha
    );
    assert_eq!(stub.lock().unwrap().repos.len(), 1);
}

#[tokio::test]
async fn a_remote_holding_another_tree_is_left_alone() {
    let root = tempfile::tempdir().unwrap();
    let (github, _stub) = serve_github(root.path()).await;
    let publisher = Publisher::new(github, reqwest::Client::new()).with_retry(no_wait());
    let run = record("d483a2f9-1111-2222-3333-444455556666");

    let first = tempfile::tempdir().unwrap();
    let bundle = make_bundle(first.path(), &[("src/main.js", b"play();\n")]).await;
    let scratch = tempfile::tempdir().unwrap();
    publisher
        .publish(&run, &bundle, scratch.path())
        .await
        .unwrap();

    let other = tempfile::tempdir().unwrap();
    let bundle = make_bundle(other.path(), &[("src/main.js", b"other();\n")]).await;
    let scratch = tempfile::tempdir().unwrap();
    let err = publisher
        .publish(&run, &bundle, scratch.path())
        .await
        .unwrap_err();
    assert!(matches!(err, PublishError::Diverged { .. }), "{err}");
}

#[tokio::test]
async fn the_first_push_after_a_create_is_retried() {
    let work = tempfile::tempdir().unwrap();
    let bundle = make_bundle(work.path(), &[("src/main.js", b"play();\n")]).await;
    let root = tempfile::tempdir().unwrap();
    let (github, stub) = serve_github(root.path()).await;
    stub.lock().unwrap().refused_pushes = 2;
    let publisher = Publisher::new(github.clone(), reqwest::Client::new()).with_retry(no_wait());
    let run = record("d483a2f9-1111-2222-3333-444455556666");
    let scratch = tempfile::tempdir().unwrap();
    assert!(
        publisher
            .publish(&run, &bundle, scratch.path())
            .await
            .unwrap()
            .is_some()
    );

    // Past the attempts, the publish fails, and a repeat completes it.
    let root = tempfile::tempdir().unwrap();
    let (github, stub) = serve_github(root.path()).await;
    stub.lock().unwrap().refused_pushes = 3;
    let publisher = Publisher::new(github, reqwest::Client::new()).with_retry(no_wait());
    let scratch = tempfile::tempdir().unwrap();
    let err = publisher
        .publish(&run, &bundle, scratch.path())
        .await
        .unwrap_err();
    assert!(
        matches!(err, PublishError::Push { attempts: 3, .. }),
        "{err}"
    );
    let scratch = tempfile::tempdir().unwrap();
    assert!(
        publisher
            .publish(&run, &bundle, scratch.path())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn a_key_is_redacted_from_every_commit_pushed() {
    let key = leaked_key();
    let source = format!("const key = \"{key}\";\n");
    let binary: &[u8] = &[0xff, 0xfe, 0x00, b's', b'k'];
    let work = tempfile::tempdir().unwrap();
    let bundle = make_bundle(
        work.path(),
        &[("src/config.js", source.as_bytes()), ("logo.bin", binary)],
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let (github, _stub) = serve_github(root.path()).await;
    let publisher = Publisher::new(github, reqwest::Client::new()).with_retry(no_wait());
    let run = record("d483a2f9-1111-2222-3333-444455556666");
    let scratch = tempfile::tempdir().unwrap();
    publisher
        .publish(&run, &bundle, scratch.path())
        .await
        .unwrap();

    let remote = remote(root.path(), &repo_name(&run));
    let everything = remote
        .run(&["log", "--all", "-p", "--format=%H %s"])
        .await
        .unwrap()
        .text();
    assert!(!everything.contains(&key), "{everything}");
    assert_eq!(
        remote
            .run(&["show", "main:src/config.js"])
            .await
            .unwrap()
            .text(),
        "const key = \"[REDACTED]\";"
    );
    assert_eq!(
        remote.run(&["show", "main:logo.bin"]).await.unwrap().stdout,
        binary
    );
    // The rewritten history keeps its messages and its shape.
    let subjects = remote
        .run(&["log", "--format=%s", "main"])
        .await
        .unwrap()
        .text();
    assert_eq!(
        subjects,
        format!("{PUBLISH_COMMIT_MESSAGE}\nImplement test case\nSeed test case")
    );

    // The rewrite is as deterministic as the publish commit.
    let again = tempfile::tempdir().unwrap();
    let prepared = prepare(&run, &bundle, again.path()).await.unwrap();
    assert_eq!(
        remote.run(&["rev-parse", "main"]).await.unwrap().text(),
        prepared.commit
    );
}

#[tokio::test]
async fn a_run_that_releases_no_source_publishes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let (github, stub) = serve_github(root.path()).await;
    let publisher = Publisher::new(github, reqwest::Client::new()).with_retry(no_wait());
    let missing = root.path().join("no.bundle");

    let mut asset = record("a1");
    asset.subject.test_type = test_cabinet_contracts::test_case::TestType::AssetGeneration;
    assert_eq!(
        publisher
            .publish(&asset, &missing, root.path())
            .await
            .unwrap(),
        None
    );
    let mut errored = record("a2");
    errored.status.state = test_cabinet_contracts::run_record::RunState::HarnessError;
    assert_eq!(
        publisher
            .publish(&errored, &missing, root.path())
            .await
            .unwrap(),
        None
    );
    assert!(stub.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn an_unusable_end_time_or_bundle_is_refused() {
    let work = tempfile::tempdir().unwrap();
    let bundle = make_bundle(work.path(), &[]).await;
    let mut run = record("r1");
    run.finished_at = "yesterday".to_string();
    let scratch = tempfile::tempdir().unwrap();
    let err = prepare(&run, &bundle, scratch.path()).await.unwrap_err();
    assert!(matches!(err, PublishError::EndTime(_)), "{err}");

    let junk = work.path().join("junk.bundle");
    std::fs::write(&junk, b"junk").unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let err = prepare(&record("r1"), &junk, scratch.path())
        .await
        .unwrap_err();
    assert!(matches!(err, PublishError::Bundle(_)), "{err}");
}

#[test]
fn the_slug_is_hosting_safe() {
    let mut run = record("D483A2F9-aaaa");
    run.subject.model_id = "Anthropic/Claude Opus 4.1".to_string();
    assert_eq!(
        repo_name(&run),
        "tcab-pong-codex-anthropic-claude-opus-4-1-d483a2f9"
    );
    let run = record("short");
    assert_eq!(run_slug(&run), "pong-codex-openai-gpt-5-4-mini-short");
}

#[test]
fn the_commit_date_is_the_instant_at_utc() {
    assert_eq!(
        commit_date("2026-10-19T12:42:17+02:00").unwrap(),
        commit_date("2026-10-19T10:42:17Z").unwrap()
    );
    assert_eq!(commit_date("1970-01-01T00:00:10Z").unwrap(), "10 +0000");
}

#[test]
fn a_person_line_splits_into_name_email_and_date() {
    assert_eq!(
        parse_person("A B <a@b.c> 1700000000 +0100"),
        Some((
            "A B".to_string(),
            "a@b.c".to_string(),
            "1700000000 +0100".to_string()
        ))
    );
    assert_eq!(parse_person("nobody"), None);
}

#[test]
fn batch_output_is_split_by_its_sizes() {
    let output = b"aaa blob 3\nxyz\nbbb blob 0\n\n";
    assert_eq!(
        parse_batch(output, 2),
        Some(vec![b"xyz".as_slice(), b"".as_slice()])
    );
    assert_eq!(parse_batch(b"aaa blob 9\nxyz\n", 1), None);
}
