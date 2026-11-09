use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::error::{InvalidScriptReason, RunpyError};

/// Private runtime validator shared by the Manager and worker builders.
///
/// Every check runs without blocking an async runtime thread: `uv` is run
/// through Tokio's process API under a deadline, and filesystem checks run on
/// the blocking pool. Nothing is cached, so a script added or removed after
/// the Manager was built is seen by the next spawn.
pub(crate) struct IntegrityChecker {
    /// Configured `uv` executable or command name.
    pub uv_path: PathBuf,
    /// Directory containing managed worker scripts.
    pub scripts_dir: PathBuf,
    /// Deadline for `uv --version`.
    pub startup_timeout: Duration,
}

impl IntegrityChecker {
    pub fn new(scripts: PathBuf, uv_path: PathBuf, startup_timeout: Duration) -> Self {
        Self {
            uv_path,
            scripts_dir: scripts,
            startup_timeout,
        }
    }

    /// Check that `uv --version` succeeds and the scripts path is a directory.
    ///
    /// The uv check requires only a successful exit; it does not parse or
    /// enforce a semantic version. A `uv` that does not exit within the
    /// startup timeout is killed and reaped.
    pub async fn check(&self) -> Result<(), RunpyError> {
        self.validate_uv().await?;
        self.validate_scripts_dir().await
    }

    async fn validate_uv(&self) -> Result<(), RunpyError> {
        let unavailable = |detail: String| RunpyError::UvUnavailable {
            path: self.uv_path.clone(),
            detail,
        };
        let mut child = match tokio::process::Command::new(&self.uv_path)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Cancelling the check (dropping its future) kills `uv`.
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Err(unavailable("executable not found".to_string()));
            }
            Err(error) => return Err(unavailable(format!("failed to execute: {error}"))),
        };

        match tokio::time::timeout(self.startup_timeout, child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => Err(unavailable(format!(
                "`--version` returned non-zero status: {status}"
            ))),
            Ok(Err(error)) => Err(unavailable(format!("failed to wait: {error}"))),
            Err(_) => {
                // `kill` also waits, so the process is reaped before returning.
                let _ = child.kill().await;
                Err(unavailable(format!(
                    "`--version` did not exit within {:?}",
                    self.startup_timeout
                )))
            }
        }
    }

    async fn validate_scripts_dir(&self) -> Result<(), RunpyError> {
        let error = |source| RunpyError::ScriptsDirectory {
            path: self.scripts_dir.clone(),
            source,
        };
        match tokio::fs::metadata(&self.scripts_dir).await {
            Ok(metadata) if metadata.is_dir() => Ok(()),
            Ok(_) => Err(error(std::io::Error::new(
                ErrorKind::NotADirectory,
                "not a directory",
            ))),
            Err(source) => Err(error(source)),
        }
    }

    /// Resolve `<scripts>/<script>.py` and its canonical scripts directory.
    ///
    /// `script` must already be a single path component. Symlinked scripts are
    /// followed: the scripts directory is trusted.
    pub async fn resolve_script(&self, script: &str) -> Result<ResolvedScript, RunpyError> {
        let scripts_dir = self.scripts_dir.clone();
        let script = script.to_string();
        blocking(move || {
            let scripts_dir =
                scripts_dir
                    .canonicalize()
                    .map_err(|source| RunpyError::ScriptsDirectory {
                        path: scripts_dir.clone(),
                        source,
                    })?;
            let script_file = scripts_dir.join(format!("{script}.py"));
            if !script_file.is_file() {
                return Err(RunpyError::InvalidScript {
                    script,
                    reason: InvalidScriptReason::NotFound(script_file),
                });
            }
            let lock_file = script_file.with_extension("py.lock");
            let locked = lock_file.is_file();
            Ok(ResolvedScript {
                scripts_dir,
                script_file,
                locked,
            })
        })
        .await
    }

    /// Absolute path for a relative multi-component `uv` path; a bare command
    /// name such as `uv` is left for the operating system to resolve.
    pub async fn resolve_uv(&self) -> Result<PathBuf, RunpyError> {
        let uv_path = self.uv_path.clone();
        if uv_path.is_absolute() || uv_path.components().count() == 1 {
            return Ok(uv_path);
        }
        blocking(move || {
            uv_path
                .canonicalize()
                .map_err(|error| RunpyError::UvUnavailable {
                    path: uv_path.clone(),
                    detail: format!("cannot resolve path: {error}"),
                })
        })
        .await
    }
}

/// A launchable script located inside the scripts directory.
pub(crate) struct ResolvedScript {
    pub scripts_dir: PathBuf,
    pub script_file: PathBuf,
    /// Whether an adjacent `<script>.py.lock` exists.
    pub locked: bool,
}

impl ResolvedScript {
    /// Parent of the scripts directory, the default working directory.
    pub fn root(&self) -> Option<&Path> {
        self.scripts_dir.parent()
    }
}

/// Run blocking filesystem work on Tokio's blocking pool.
async fn blocking<T, F>(work: F) -> Result<T, RunpyError>
where
    F: FnOnce() -> Result<T, RunpyError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| RunpyError::Io(std::io::Error::other(error)))?
}
