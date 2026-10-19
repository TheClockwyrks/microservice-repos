//! Running `git`, isolated from whatever configuration the host carries.
//!
//! Every command the service runs reads no system or global configuration and
//! never prompts, so a bundle check and a publish behave the same on the image,
//! in a test and on a developer's machine: no host `commit.gpgSign`, no
//! credential helper, no `init.defaultBranch`, no hook template.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// A `git` command failed to run or exited unsuccessfully.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// The process could not be started or its pipes not used.
    #[error("running `git {args}`: {source}")]
    Spawn {
        /// The arguments, space-joined.
        args: String,
        /// Why it could not run.
        source: std::io::Error,
    },
    /// The process exited unsuccessfully.
    #[error("`git {args}` failed: {stderr}")]
    Failed {
        /// The arguments, space-joined.
        args: String,
        /// What it wrote to standard error, trimmed.
        stderr: String,
    },
}

/// What a successful command wrote.
#[derive(Debug, Clone)]
pub struct GitOutput {
    /// Standard output, as bytes.
    pub stdout: Vec<u8>,
}

impl GitOutput {
    /// Standard output as text, trimmed.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
}

/// `git`, run in one directory with an isolated configuration.
#[derive(Debug, Clone)]
pub struct Git {
    dir: PathBuf,
    env: Vec<(String, String)>,
}

impl Git {
    /// `git` run in `dir`. `home` is the `HOME` it sees, a directory of the
    /// caller's own that holds no configuration.
    pub fn new(dir: &Path, home: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            env: vec![
                ("HOME".to_string(), home.display().to_string()),
                ("XDG_CONFIG_HOME".to_string(), home.display().to_string()),
                ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
                ("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string()),
                ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
                ("GIT_ASKPASS".to_string(), "true".to_string()),
                ("LC_ALL".to_string(), "C".to_string()),
            ],
        }
    }

    /// This `git` with `env` added to every command's environment.
    pub fn with_env(&self, env: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut git = self.clone();
        git.env.extend(env);
        git
    }

    /// Run `git <args>` and require it to succeed.
    pub async fn run(&self, args: &[&str]) -> Result<GitOutput, GitError> {
        self.run_with_input(args, None).await
    }

    /// Run `git <args>` with `input` on its standard input, and require it to
    /// succeed. The input is written while the output is read, so a command
    /// that answers as it reads (`cat-file --batch`) cannot deadlock on a full
    /// pipe.
    pub async fn run_with_input(
        &self,
        args: &[&str],
        input: Option<Vec<u8>>,
    ) -> Result<GitOutput, GitError> {
        let joined = args.join(" ");
        let spawn_error = |source| GitError::Spawn {
            args: joined.clone(),
            source,
        };
        let mut command = tokio::process::Command::new("git");
        command
            .args(args)
            .current_dir(&self.dir)
            // A location the service's own environment carries (a commit hook's
            // `GIT_DIR`) would point git somewhere else than `dir`; one this
            // `Git` sets itself is put back below.
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(spawn_error)?;
        let writer = match (input, child.stdin.take()) {
            (Some(input), Some(mut stdin)) => Some(tokio::spawn(async move {
                stdin.write_all(&input).await?;
                stdin.shutdown().await
            })),
            _ => None,
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = child.stdout.take().expect("stdout is piped");
        let mut err = child.stderr.take().expect("stderr is piped");
        let (read_out, read_err) =
            tokio::join!(out.read_to_end(&mut stdout), err.read_to_end(&mut stderr));
        read_out.map_err(spawn_error)?;
        read_err.map_err(spawn_error)?;
        let status = child.wait().await.map_err(spawn_error)?;
        if let Some(writer) = writer {
            // A command that exits before reading all its input closes the pipe;
            // its exit status, not the broken pipe, is the outcome.
            let _ = writer.await;
        }
        if !status.success() {
            return Err(GitError::Failed {
                args: joined,
                stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
            });
        }
        Ok(GitOutput { stdout })
    }
}

#[cfg(test)]
#[path = "git.test.rs"]
mod tests;
