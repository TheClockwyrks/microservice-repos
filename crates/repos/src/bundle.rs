//! An uploaded bundle: staged on disk as it arrives, then checked to be a bundle
//! a publish can push.
//!
//! The driver writes `git bundle --all` of the run's repository, so a bundle the
//! service accepts names `refs/heads/main` and needs no commit it does not
//! carry. Anything else is the run's own failure (`422`): it would store, and
//! fail only later, at publish, when nothing can be done about it.

use std::path::{Path, PathBuf};

use axum::body::Body;
use futures_util::StreamExt as _;
use tokio::io::AsyncWriteExt as _;

use crate::git::{Git, GitError};

/// The largest bundle accepted, in bytes. A bundle is the run's source history
/// with its build trees left out by the seeded `.gitignore`, so a legitimate one
/// is far smaller; the cap bounds what one upload can make the service hold on
/// its disk.
pub const MAX_BUNDLE_BYTES: u64 = 1024 * 1024 * 1024;

/// The branch a bundle must carry and a publish pushes.
pub const MAIN: &str = "refs/heads/main";

/// Staging or checking an upload failed.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    /// The body is longer than the cap.
    #[error("the bundle exceeds the {limit}-byte cap")]
    TooLarge {
        /// The cap, in bytes.
        limit: u64,
    },
    /// The body is not a bundle a publish can push.
    #[error("the upload is not a usable bundle: {0}")]
    Invalid(String),
    /// The body failed to arrive.
    #[error("reading the upload: {0}")]
    Body(String),
    /// The staged file could not be written, or git could not run.
    #[error("staging the bundle: {0}")]
    Io(String),
}

/// Write `body` to `<dir>/upload.bundle` as it arrives, refusing it once it
/// passes `limit` bytes, and answer the file's path and length.
pub async fn stage(body: Body, limit: u64, dir: &Path) -> Result<(PathBuf, u64), BundleError> {
    let path = dir.join("upload.bundle");
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(|err| BundleError::Io(err.to_string()))?;
    let mut length = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| BundleError::Body(err.to_string()))?;
        length += chunk.len() as u64;
        if length > limit {
            return Err(BundleError::TooLarge { limit });
        }
        file.write_all(&chunk)
            .await
            .map_err(|err| BundleError::Io(err.to_string()))?;
    }
    file.flush()
        .await
        .map_err(|err| BundleError::Io(err.to_string()))?;
    Ok((path, length))
}

/// Check that the file at `bundle` is a git bundle carrying [`MAIN`] that
/// applies to an empty repository, which is what `git bundle --all` of a run's
/// repository writes. `scratch` is an empty directory the check may use.
pub async fn check(bundle: &Path, scratch: &Path) -> Result<(), BundleError> {
    let bundle_arg = bundle.display().to_string();
    let repo = scratch.join("check.git");
    std::fs::create_dir_all(&repo).map_err(|err| BundleError::Io(err.to_string()))?;
    let git = Git::new(&repo, scratch);
    let heads = git
        .run(&["bundle", "list-heads", &bundle_arg])
        .await
        .map_err(invalid)?
        .text();
    if !heads
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(MAIN))
    {
        return Err(BundleError::Invalid(format!(
            "it carries no `{MAIN}` (write it with `git bundle create <file> --all` on the run's \
             repository)"
        )));
    }
    git.run(&["init", "--quiet", "--bare"])
        .await
        .map_err(|err| BundleError::Io(err.to_string()))?;
    git.run(&["bundle", "verify", "--quiet", &bundle_arg])
        .await
        .map_err(invalid)?;
    Ok(())
}

/// A git refusal of the bundle, as the run's own failure.
fn invalid(err: GitError) -> BundleError {
    match err {
        GitError::Failed { stderr, .. } => BundleError::Invalid(stderr),
        GitError::Spawn { .. } => BundleError::Io(err.to_string()),
    }
}

#[cfg(test)]
#[path = "bundle.test.rs"]
mod tests;
