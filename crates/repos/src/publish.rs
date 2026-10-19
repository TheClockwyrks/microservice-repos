//! Publishing a run's implementation: its public repository, created from the
//! stored bundle.
//!
//! See the repos service's Publishing section in the documentation site
//! (`components/repos/overview.md`). In order:
//!
//! 1. The bundle's `main` is fetched into a scratch bare repository.
//! 2. Every file the bundle carries is run through the shared secret scrubber,
//!    in every commit of its history, since a push of `main` sends them all
//!    (see [`prepare`]), and the scrubbed tree is committed on top of the
//!    tip. The commit is deterministic: its message is [`PUBLISH_COMMIT_MESSAGE`], its author and
//!    committer are [`AUTHOR_NAME`] `<`[`AUTHOR_EMAIL`]`>`, and both its dates
//!    are the run's end time from its record, so a repeat over the same bundle
//!    reproduces the same SHA.
//! 3. The run's repository, `tcab-<slug>`, is created in the organization, or
//!    reused when it exists.
//! 4. The commit is pushed as `main` with a plain push. A remote whose `main`
//!    already holds the same tree is a success whatever commit carries it, and
//!    the first push after a create is retried with backoff, so GitHub's
//!    permission-propagation lag after a create heals itself.
//!
//! Every step is safe to repeat, so a publish repeated after a failure at any
//! step completes it, and answers the same URL.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::time::Duration;

use test_cabinet_contracts::run_record::RunRecord;
use test_cabinet_redact::SecretScrubber;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::bundle::MAIN;
use crate::git::{Git, GitError};
use crate::github::{GitHubConfig, GitHubError, ensure_repo};

/// The message of the commit that carries the scrubbed tree.
pub const PUBLISH_COMMIT_MESSAGE: &str = "Publish the run's implementation";
/// The author and committer name of that commit.
pub const AUTHOR_NAME: &str = "Clockwyrks";
/// The author and committer email of that commit.
pub const AUTHOR_EMAIL: &str = "runs@clockwyrks.invalid";
/// The fixed prefix of every run's repository name.
pub const REPO_PREFIX: &str = "tcab-";

/// A publish failed.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    /// The stored bundle cannot be unpacked, or carries no `main`.
    #[error("the stored bundle cannot be published: {0}")]
    Bundle(String),
    /// The record carries no end time a commit can be dated with.
    #[error("the run's record has no usable end time `{0}`")]
    EndTime(String),
    /// A local git step failed.
    #[error("{0}")]
    Git(#[from] GitError),
    /// The repository could not be looked up or created.
    #[error("{0}")]
    GitHub(#[from] GitHubError),
    /// The remote's `main` holds a different tree, which a plain push may not
    /// replace.
    #[error(
        "`{repo}` already holds a `main` whose tree differs from this run's scrubbed \
         implementation; it is left as it is"
    )]
    Diverged {
        /// The repository.
        repo: String,
    },
    /// Every push attempt failed.
    #[error("pushing to `{repo}` failed after {attempts} attempt(s): {detail}")]
    Push {
        /// The repository.
        repo: String,
        /// How many pushes were made.
        attempts: u32,
        /// The last attempt's failure.
        detail: String,
    },
    /// The scratch directory could not be used.
    #[error("the publish's scratch directory: {0}")]
    Io(String),
}

/// How the first push after a create rides out GitHub's permission-propagation
/// lag: the create answers as soon as the repository exists, but the creator's
/// write grant is not always live on the replica that serves the push a moment
/// later, so an immediate push can be refused. A short settle before the first
/// push and bounded retries with doubling backoff cover it. A push to a
/// repository that already existed is made once.
#[derive(Debug, Clone, Copy)]
pub struct PushRetry {
    /// Total push attempts after a create (at least one).
    pub attempts: u32,
    /// The pause before the first push of a just-created repository.
    pub initial_delay: Duration,
    /// The pause before the first retry, doubled before each later one.
    pub backoff: Duration,
}

impl Default for PushRetry {
    fn default() -> Self {
        // A 3 s settle, then up to four retries (1, 2, 4, 8 s): about 18 s,
        // comfortably past the propagation lag and well inside a publish Job.
        Self {
            attempts: 5,
            initial_delay: Duration::from_secs(3),
            backoff: Duration::from_secs(1),
        }
    }
}

/// Publishes runs to the configured organization.
#[derive(Debug, Clone)]
pub struct Publisher {
    github: GitHubConfig,
    http: reqwest::Client,
    retry: PushRetry,
}

/// What a publish prepared locally: the commit to push and its tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// The deterministic commit carrying the scrubbed tree.
    pub commit: String,
    /// Its tree.
    pub tree: String,
}

impl Publisher {
    /// A publisher to `github`, sending its API calls with `http`.
    pub fn new(github: GitHubConfig, http: reqwest::Client) -> Self {
        Self {
            github,
            http,
            retry: PushRetry::default(),
        }
    }

    /// This publisher with another push retry policy.
    pub fn with_retry(mut self, retry: PushRetry) -> Self {
        self.retry = retry;
        self
    }

    /// The organization and endpoints this publisher publishes to.
    pub fn github(&self) -> &GitHubConfig {
        &self.github
    }

    /// Publish the run `record` describes from the bundle at `bundle`, working
    /// in the empty directory `scratch`. Answers the repository's URL, or `None`
    /// for a run that releases no source: an asset-generation run, whose output
    /// is its recorded operation log, and a run whose state releases nothing.
    #[tracing::instrument(name = "repos.publish", skip_all, fields(run.id = %record.id), err)]
    pub async fn publish(
        &self,
        record: &RunRecord,
        bundle: &Path,
        scratch: &Path,
    ) -> Result<Option<String>, PublishError> {
        if !releases_source(record) {
            return Ok(None);
        }
        let prepared = prepare(record, bundle, scratch).await?;
        let name = repo_name(record);
        let created = ensure_repo(&self.http, &self.github, &name, &description(record)).await?;
        self.push(scratch, &name, &prepared, created).await?;
        tracing::info!(
            repo = %name,
            commit = %prepared.commit,
            created,
            "published the run's implementation"
        );
        Ok(Some(self.github.repo_url(&name)))
    }

    /// Push `prepared.commit` as `main` of repository `name`. A refused push
    /// whose remote `main` already holds the prepared tree is a success. After a
    /// create, a refusal is retried with backoff (see [`PushRetry`]); a remote
    /// `main` with a different tree is never overwritten.
    async fn push(
        &self,
        scratch: &Path,
        name: &str,
        prepared: &Prepared,
        created: bool,
    ) -> Result<(), PublishError> {
        let git =
            Git::new(&scratch.join("publish.git"), scratch).with_env(self.github.git_auth_env());
        let remote = self.github.remote_url(name);
        let refspec = format!("{}:{MAIN}", prepared.commit);
        let attempts = if created {
            self.retry.attempts.max(1)
        } else {
            1
        };
        if created && !self.retry.initial_delay.is_zero() {
            tokio::time::sleep(self.retry.initial_delay).await;
        }
        let mut backoff = self.retry.backoff;
        let mut detail = String::new();
        for attempt in 1..=attempts {
            match git.run(&["push", "--quiet", &remote, &refspec]).await {
                Ok(_) => return Ok(()),
                Err(err) => detail = err.to_string(),
            }
            match remote_main_tree(&git, &remote).await {
                Some(tree) if tree == prepared.tree => return Ok(()),
                Some(_) => {
                    return Err(PublishError::Diverged {
                        repo: format!("{}/{name}", self.github.org),
                    });
                }
                None => {}
            }
            if attempt < attempts {
                tracing::warn!(
                    repo = %name,
                    attempt,
                    error = %detail,
                    "the push was refused; retrying after backoff (GitHub's permission \
                     propagation after a create)"
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2);
            }
        }
        Err(PublishError::Push {
            repo: format!("{}/{name}", self.github.org),
            attempts,
            detail,
        })
    }
}

/// The tree the remote's `main` holds, or `None` when it has no `main` or
/// cannot be reached.
async fn remote_main_tree(git: &Git, remote: &str) -> Option<String> {
    git.run(&["fetch", "--quiet", "--no-tags", remote, MAIN])
        .await
        .ok()?;
    git.run(&["rev-parse", "--verify", "FETCH_HEAD^{tree}"])
        .await
        .ok()
        .map(|output| output.text())
}

/// Whether the run releases its source as a repository: a code-writing run in a
/// state whose artifacts are released.
pub fn releases_source(record: &RunRecord) -> bool {
    record.status.state.publishes_artifacts() && record.subject.test_type.releases_source_repo()
}

/// Fetch the bundle's `main` into `<scratch>/publish.git`, scrub every file of
/// its history, and commit the scrubbed tree on top of the scrubbed tip,
/// deterministically.
///
/// Every commit the push sends is scrubbed, not the tip's tree alone: a key the
/// model wrote reaches the bundle's implementation commit, and a push of `main`
/// sends the whole history, so a commit left as it was would publish the key
/// the scrubbed tree removed. A commit whose files hold no key, and whose
/// parents are kept, is kept as it is, so a bundle with nothing to redact is
/// pushed with its own commits and the publish commit on top of its own tip. A
/// commit that changes is recreated with its own message, author, committer
/// and dates, so the rewrite is as deterministic as the publish commit.
pub async fn prepare(
    record: &RunRecord,
    bundle: &Path,
    scratch: &Path,
) -> Result<Prepared, PublishError> {
    let date = commit_date(&record.finished_at)?;
    let repo = scratch.join("publish.git");
    std::fs::create_dir_all(&repo).map_err(|err| PublishError::Io(err.to_string()))?;
    let git = Git::new(&repo, scratch);
    git.run(&["init", "--quiet", "--bare"]).await?;
    let bundle_arg = bundle.display().to_string();
    git.run(&[
        "fetch",
        "--quiet",
        "--no-tags",
        &bundle_arg,
        &format!("{MAIN}:{MAIN}"),
    ])
    .await
    .map_err(|err| PublishError::Bundle(err.to_string()))?;

    let mut history = History {
        git: git.clone(),
        index: scratch.join("publish.index"),
        scrubber: SecretScrubber::from_host_env(),
        blobs: HashMap::new(),
        commits: HashMap::new(),
        redacted: BTreeSet::new(),
    };
    let tip = history.rewrite(MAIN).await?;
    if !history.redacted.is_empty() {
        tracing::warn!(
            files = ?history.redacted,
            "redacted leaked API key(s) from a run's implementation before publishing it"
        );
    }

    let tree = git
        .run(&["rev-parse", "--verify", &format!("{tip}^{{tree}}")])
        .await?
        .text();
    let commit = git
        .with_env(identity(
            (AUTHOR_NAME, AUTHOR_EMAIL, date.as_str()),
            (AUTHOR_NAME, AUTHOR_EMAIL, date.as_str()),
        ))
        .run(&[
            "-c",
            "commit.gpgSign=false",
            "commit-tree",
            &tree,
            "-p",
            &tip,
            "-m",
            PUBLISH_COMMIT_MESSAGE,
        ])
        .await?
        .text();
    Ok(Prepared { commit, tree })
}

/// The `GIT_AUTHOR_*` and `GIT_COMMITTER_*` environment of a commit by
/// `author` and `committer`, each a name, an email and a date.
fn identity(author: (&str, &str, &str), committer: (&str, &str, &str)) -> [(String, String); 6] {
    [
        ("GIT_AUTHOR_NAME", author.0),
        ("GIT_AUTHOR_EMAIL", author.1),
        ("GIT_AUTHOR_DATE", author.2),
        ("GIT_COMMITTER_NAME", committer.0),
        ("GIT_COMMITTER_EMAIL", committer.1),
        ("GIT_COMMITTER_DATE", committer.2),
    ]
    .map(|(key, value)| (key.to_string(), value.to_string()))
}

/// The scrub of one bundle's history.
struct History {
    git: Git,
    /// A scratch index file a changed tree is built in.
    index: std::path::PathBuf,
    scrubber: SecretScrubber,
    /// Every regular file's blob read so far, to its scrubbed replacement, or
    /// `None` when the scrubber leaves it as it is.
    blobs: HashMap<String, Option<String>>,
    /// Every commit rewritten so far, to the commit that replaces it (itself
    /// when nothing in it changed).
    commits: HashMap<String, String>,
    /// The paths of the files a key was redacted from.
    redacted: BTreeSet<String>,
}

impl History {
    /// Scrub every commit reachable from `rev`, parents first, and answer the
    /// commit that replaces `rev`.
    async fn rewrite(&mut self, rev: &str) -> Result<String, PublishError> {
        let listing = self
            .git
            .run(&["rev-list", "--reverse", "--topo-order", "--parents", rev])
            .await?
            .text();
        let mut last = None;
        for line in listing.lines() {
            let mut shas = line.split_whitespace();
            let Some(commit) = shas.next() else {
                continue;
            };
            let parents: Vec<String> = shas
                .map(|parent| {
                    self.commits
                        .get(parent)
                        .cloned()
                        .unwrap_or_else(|| parent.to_string())
                })
                .collect();
            let parents_kept = line
                .split_whitespace()
                .skip(1)
                .eq(parents.iter().map(String::as_str));
            let (tree, changed) = self.scrub_tree(commit).await?;
            let replacement = if !changed && parents_kept {
                commit.to_string()
            } else {
                self.recreate(commit, &tree, &parents).await?
            };
            self.commits.insert(commit.to_string(), replacement.clone());
            last = Some(replacement);
        }
        last.ok_or_else(|| PublishError::Bundle(format!("`{rev}` has no commits")))
    }

    /// The scrubbed tree of `commit`, and whether it differs from the commit's
    /// own. Binary and non-UTF-8 files, symbolic links and submodule entries
    /// are kept as they are, since a key is pasted as text.
    async fn scrub_tree(&mut self, commit: &str) -> Result<(String, bool), PublishError> {
        let own = self
            .git
            .run(&["rev-parse", "--verify", &format!("{commit}^{{tree}}")])
            .await?
            .text();
        let listing = self
            .git
            .run(&["ls-tree", "-r", "-z", "--full-tree", commit])
            .await?;
        let mut files = Vec::new();
        for entry in listing.stdout.split(|byte| *byte == 0) {
            let Some(tab) = entry.iter().position(|byte| *byte == b'\t') else {
                continue;
            };
            let meta = String::from_utf8_lossy(&entry[..tab]).to_string();
            let mut fields = meta.split(' ');
            let (Some(mode), Some(kind), Some(sha)) = (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if kind == "blob" && (mode == "100644" || mode == "100755") {
                files.push((mode.to_string(), sha.to_string(), entry[tab + 1..].to_vec()));
            }
        }
        self.read_blobs(files.iter().map(|(_, sha, _)| sha.as_str()))
            .await?;

        let mut updates = Vec::new();
        for (mode, sha, path) in &files {
            if let Some(Some(replacement)) = self.blobs.get(sha) {
                updates.extend_from_slice(format!("{mode} {replacement}\t").as_bytes());
                updates.extend_from_slice(path);
                updates.push(0);
                self.redacted
                    .insert(String::from_utf8_lossy(path).to_string());
            }
        }
        if updates.is_empty() {
            return Ok((own, false));
        }
        let indexed = self.git.with_env([(
            "GIT_INDEX_FILE".to_string(),
            self.index.display().to_string(),
        )]);
        indexed.run(&["read-tree", commit]).await?;
        indexed
            .run_with_input(&["update-index", "-z", "--index-info"], Some(updates))
            .await?;
        let tree = indexed.run(&["write-tree"]).await?.text();
        let changed = tree != own;
        Ok((tree, changed))
    }

    /// Read and scrub every blob of `shas` not read yet, writing the scrubbed
    /// text of each the scrubber changes as a new blob.
    async fn read_blobs<'a>(
        &mut self,
        shas: impl Iterator<Item = &'a str>,
    ) -> Result<(), PublishError> {
        let mut unread: Vec<&str> = shas.filter(|sha| !self.blobs.contains_key(*sha)).collect();
        unread.sort_unstable();
        unread.dedup();
        if unread.is_empty() {
            return Ok(());
        }
        let mut request = Vec::new();
        for sha in &unread {
            request.extend_from_slice(sha.as_bytes());
            request.push(b'\n');
        }
        let batch = self
            .git
            .run_with_input(&["cat-file", "--batch"], Some(request))
            .await?;
        let contents = parse_batch(&batch.stdout, unread.len()).ok_or_else(|| {
            PublishError::Bundle("unreadable `cat-file --batch` output".to_string())
        })?;
        for (sha, content) in unread.into_iter().zip(contents) {
            let replacement =
                match std::str::from_utf8(content).map(|text| self.scrubber.scrub(text)) {
                    Ok(Cow::Owned(scrubbed)) => Some(
                        self.git
                            .run_with_input(
                                &["hash-object", "-w", "--stdin"],
                                Some(scrubbed.into_bytes()),
                            )
                            .await?
                            .text(),
                    ),
                    _ => None,
                };
            self.blobs.insert(sha.to_string(), replacement);
        }
        Ok(())
    }

    /// A commit like `commit`, with its message, author, committer and dates,
    /// over `tree` with `parents`.
    async fn recreate(
        &self,
        commit: &str,
        tree: &str,
        parents: &[String],
    ) -> Result<String, PublishError> {
        let raw = self.git.run(&["cat-file", "commit", commit]).await?.stdout;
        let split = raw
            .windows(2)
            .position(|pair| pair == b"\n\n")
            .unwrap_or(raw.len());
        let header = String::from_utf8_lossy(&raw[..split]).to_string();
        let message = raw.get(split + 2..).unwrap_or_default().to_vec();
        let person = |field: &str| {
            header
                .lines()
                .find_map(|line| line.strip_prefix(field))
                .and_then(parse_person)
                .ok_or_else(|| {
                    PublishError::Bundle(format!("commit {commit} has no readable `{field}`"))
                })
        };
        let author = person("author ")?;
        let committer = person("committer ")?;
        let encoding = header
            .lines()
            .find_map(|line| line.strip_prefix("encoding "))
            .unwrap_or("UTF-8")
            .to_string();
        let encoding_config = format!("i18n.commitEncoding={encoding}");
        let mut args = vec![
            "-c",
            "commit.gpgSign=false",
            "-c",
            encoding_config.as_str(),
            "commit-tree",
            tree,
        ];
        for parent in parents {
            args.push("-p");
            args.push(parent);
        }
        args.extend(["-F", "-"]);
        Ok(self
            .git
            .with_env(identity(
                (&author.0, &author.1, &author.2),
                (&committer.0, &committer.1, &committer.2),
            ))
            .run_with_input(&args, Some(message))
            .await?
            .text())
    }
}

/// The name, email and date of a commit's `author` or `committer` value,
/// `Name <email> <seconds> <offset>`.
fn parse_person(value: &str) -> Option<(String, String, String)> {
    let open = value.find(" <")?;
    let close = value[open..].find('>')? + open;
    Some((
        value[..open].to_string(),
        value[open + 2..close].to_string(),
        value[close + 1..].trim().to_string(),
    ))
}

/// Split `cat-file --batch` output into the `expected` objects' contents, in
/// order, or `None` when it is not `expected` well-formed objects.
fn parse_batch(output: &[u8], expected: usize) -> Option<Vec<&[u8]>> {
    let mut contents = Vec::with_capacity(expected);
    let mut rest = output;
    for _ in 0..expected {
        let newline = rest.iter().position(|byte| *byte == b'\n')?;
        let header = std::str::from_utf8(&rest[..newline]).ok()?;
        let size: usize = header.rsplit(' ').next()?.parse().ok()?;
        let start = newline + 1;
        let end = start.checked_add(size)?;
        contents.push(rest.get(start..end)?);
        // Each object's content is followed by one newline.
        rest = rest.get(end + 1..)?;
    }
    Some(contents)
}

/// The `GIT_*_DATE` of an RFC 3339 `finished_at`: seconds since the epoch at
/// UTC, git's internal format, so the commit's bytes depend on the instant
/// alone.
pub fn commit_date(finished_at: &str) -> Result<String, PublishError> {
    let at = OffsetDateTime::parse(finished_at, &Rfc3339)
        .map_err(|_| PublishError::EndTime(finished_at.to_string()))?;
    Ok(format!("{} +0000", at.unix_timestamp()))
}

/// The run's repository name, for example `tcab-pong-codex-gpt-5-4-mini-d483a2f9`.
pub fn repo_name(record: &RunRecord) -> String {
    format!("{REPO_PREFIX}{}", run_slug(record))
}

/// The repository's description.
fn description(record: &RunRecord) -> String {
    format!(
        "A run of {} by {} through {} in The Test Cabinet (run {}).",
        record.subject.test_case_slug,
        record.subject.model_id,
        record.subject.harness_slug.as_str(),
        record.id
    )
}

/// A run's stable, hosting-safe slug: `<test-case>-<harness>-<model>-<short-id>`
/// reduced to lowercase `[a-z0-9-]`, where the short id is the first segment of
/// the run id, at most eight characters. It keeps an organization browsable
/// while the short id keeps it unique, and it is the slug the publisher named
/// repositories with before the repos service took the push over, so a run
/// published then keeps its repository.
pub fn run_slug(record: &RunRecord) -> String {
    let short_id: String = record
        .id
        .split('-')
        .next()
        .unwrap_or(&record.id)
        .chars()
        .take(8)
        .collect();
    sanitize_label(&format!(
        "{}-{}-{}-{}",
        record.subject.test_case_slug,
        record.subject.harness_slug.as_str(),
        record.subject.model_id,
        short_id,
    ))
}

/// Reduce text to a lowercase `[a-z0-9-]` token with no leading, trailing or
/// repeated hyphens.
fn sanitize_label(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_hyphen = false;
    for raw in input.chars() {
        let lowered = raw.to_ascii_lowercase();
        if lowered.is_ascii_alphanumeric() {
            out.push(lowered);
            prev_hyphen = false;
        } else if !prev_hyphen {
            out.push('-');
            prev_hyphen = true;
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
#[path = "publish.test.rs"]
mod tests;
