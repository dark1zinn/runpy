use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use crate::scribbler::scribbler;

pub struct IntegrityChecker {
    pub uv_path: PathBuf,
    pub scripts_dir: PathBuf,
    pub registry: Mutex<HashSet<String>>,
}

impl IntegrityChecker {
    pub fn new(scripts: &str, uv_path: &str) -> Self {
        Self {
            uv_path: PathBuf::from(uv_path),
            scripts_dir: PathBuf::from(scripts),
            registry: Mutex::new(HashSet::new()),
        }
    }

    /// Run all integrity checks: validate uv, ensure the socket and scripts
    /// directories exist, and index available scripts.
    pub fn perform_check(&self) -> Result<(), String> {
        self.validate_uv()?;

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

    /// Walk the scripts directory (recursively) and index all `.py` files by
    /// their stem name.
    fn index_scripts(&self) {
        let mut scripts = self.registry.lock().unwrap();
        scripts.clear();

        self.walk_dir(&self.scripts_dir, &mut scripts);

        scribbler().debug_with(
            "Integrity",
            &format!("Indexed {} scripts: {:?}", scripts.len(), scripts),
        );
    }

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
