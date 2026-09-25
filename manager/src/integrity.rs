use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use crate::scribbler::Scribbler;

/// Private runtime validator shared by Manager and worker facades.
///
/// It retains configured paths and a diagnostic index; spawn still validates
/// the exact root-level script path before binding a socket.
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
    pub fn new(scripts: &str, uv_path: &str, logger: Arc<Scribbler>) -> Self {
        Self {
            uv_path: PathBuf::from(uv_path),
            scripts_dir: PathBuf::from(scripts),
            registry: Mutex::new(HashSet::new()),
            logger,
        }
    }

    /// Validate that uv executes successfully, ensure `/tmp/runpy` exists,
    /// verify the scripts directory, and refresh the diagnostic script index.
    ///
    /// The uv check requires only a successful `--version` exit; it does not
    /// parse or enforce a semantic version.
    pub fn perform_check(&self) -> Result<(), String> {
        self.validate_uv()?;

        // Socket creation is centralized under a stable Manager-owned
        // directory; individual Worker spawns own their socket file cleanup.
        // Ensure socket directory exists
        let sock_dir = PathBuf::from("/tmp/runpy");
        if !sock_dir.exists() {
            std::fs::create_dir_all(&sock_dir)
                .map_err(|e| format!("Failed to create socket directory: {}", e))?;
        }

        // Validate scripts directory exists
        if !self.scripts_dir.exists() {
            return Err(format!(
                "Scripts directory does not exist: '{}'",
                self.scripts_dir.display()
            ));
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

    fn validate_uv(&self) -> Result<(), String> {
        match Command::new(&self.uv_path).arg("--version").status() {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!(
                "uv executable at '{}' returned non-zero status: {}",
                self.uv_path.display(),
                status
            )),
            Err(error) if error.kind() == ErrorKind::NotFound => Err(format!(
                "uv executable not found at '{}'",
                self.uv_path.display()
            )),
            Err(error) => Err(format!(
                "Failed to execute uv at '{}': {}",
                self.uv_path.display(),
                error
            )),
        }
    }

    /// Refresh the recursive script-stem index used for diagnostics.
    ///
    /// Spawn separately checks the requested root-level `<name>.py`, so this
    /// index must not be treated as authorization to launch a nested file.
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
