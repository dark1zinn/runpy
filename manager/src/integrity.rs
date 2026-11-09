use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::error::RunpyError;
use crate::scribbler::Scribbler;

/// Private runtime validator shared by Manager and worker facades.
///
/// It retains configured paths and a diagnostic index; Worker::spawn validates
/// script names before selecting a file.
pub struct IntegrityChecker {
    /// Configured `uv` executable or command name.
    pub uv_path: PathBuf,
    /// Directory containing managed worker scripts.
    pub scripts_dir: PathBuf,
    /// Recursively indexed non-dunder Python file stems.
    pub registry: Mutex<HashSet<String>>,
    logger: Arc<Scribbler>,
}

impl IntegrityChecker {
    pub fn new(scripts: PathBuf, uv_path: PathBuf, logger: Arc<Scribbler>) -> Self {
        Self {
            uv_path,
            scripts_dir: scripts,
            registry: Mutex::new(HashSet::new()),
            logger,
        }
    }

    /// Validate that uv executes successfully, verify the scripts directory,
    /// and refresh the diagnostic script index.
    ///
    /// The uv check requires only a successful `--version` exit; it does not
    /// parse or enforce a semantic version.
    pub fn perform_check(&self) -> Result<(), RunpyError> {
        self.validate_uv()?;

        match std::fs::metadata(&self.scripts_dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(RunpyError::ScriptsDirectory {
                    path: self.scripts_dir.clone(),
                    source: std::io::Error::new(ErrorKind::NotADirectory, "not a directory"),
                });
            }
            Err(source) => {
                return Err(RunpyError::ScriptsDirectory {
                    path: self.scripts_dir.clone(),
                    source,
                });
            }
        }

        // Index Scripts
        self.index_scripts();

        Ok(())
    }

    /// Check if a specific script exists in the registry. Re-indexes first.
    #[allow(dead_code)]
    pub fn check_script(&self, script: &str) -> bool {
        self.index_scripts();
        let scripts = self.registry.lock().unwrap();
        scripts.contains(script)
    }

    fn validate_uv(&self) -> Result<(), RunpyError> {
        let unavailable = |detail: String| RunpyError::UvUnavailable {
            path: self.uv_path.clone(),
            detail,
        };
        match Command::new(&self.uv_path)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(unavailable(format!(
                "`--version` returned non-zero status: {status}"
            ))),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                Err(unavailable("executable not found".to_string()))
            }
            Err(error) => Err(unavailable(format!("failed to execute: {error}"))),
        }
    }

    /// Refresh the recursive script-stem index used for diagnostics.
    ///
    /// This index is diagnostic and does not authorize script selection.
    fn index_scripts(&self) {
        let mut scripts = self.registry.lock().unwrap();
        scripts.clear();

        self.walk_dir(&self.scripts_dir, &mut scripts);

        self.logger.debug_with(
            "Integrity",
            &format!("Indexed {} scripts: {:?}", scripts.len(), scripts),
        );
    }

    /// Add readable `.py` stems recursively, excluding dunder files.
    ///
    /// Unreadable entries and non-UTF-8 stems are skipped so diagnostics cannot
    /// make otherwise valid manager construction fail.
    fn walk_dir(&self, dir: &PathBuf, scripts: &mut HashSet<String>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    self.walk_dir(&path, scripts);
                } else if path.extension().and_then(|s| s.to_str()) == Some("py") {
                    let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    // Skip __init__.py and other dunder files
                    if !name.starts_with("__") {
                        scripts.insert(name.to_string());
                    }
                }
            }
        }
    }
}
