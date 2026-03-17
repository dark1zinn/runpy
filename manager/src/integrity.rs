use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

pub struct IntegrityChecker {
    pub venv_path: PathBuf,
    pub scripts_dir: PathBuf,
    pub registry: Mutex<HashSet<String>>,
}

impl IntegrityChecker {
    pub fn new(venv: &str, scripts: &str) -> Self {
        Self {
            venv_path: PathBuf::from(venv),
            scripts_dir: PathBuf::from(scripts),
            registry: Mutex::new(HashSet::new()),
        }
    }

    /// Run all integrity checks: validate the venv, ensure the socket directory
    /// exists, and index available scripts. Returns `Err` on any failure instead
    /// of panicking.
    pub fn perform_check(&self) -> Result<(), String> {
        // Validate Venv
        if !self.validate_venv() {
            return Err(format!(
                "Python executable missing in venv at '{}'",
                self.venv_path.display()
            ));
        }

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
    pub fn check_script(&self, script: &str) -> bool {
        self.index_scripts();
        let scripts = self.registry.lock().unwrap();
        scripts.contains(script)
    }

    /// Validate that the Python venv has a valid python executable.
    fn validate_venv(&self) -> bool {
        let py_bin = if cfg!(windows) {
            "Scripts/python.exe"
        } else {
            "bin/python"
        };
        self.venv_path.join(py_bin).exists()
    }

    /// Walk the scripts directory (recursively) and index all `.py` files by
    /// their stem name.
    fn index_scripts(&self) {
        let mut scripts = self.registry.lock().unwrap();
        scripts.clear();

        self.walk_dir(&self.scripts_dir, &mut scripts);

        println!(
            "Indexed scripts: {:?} \n  - {:?}",
            scripts.len(),
            scripts
        );
    }

    fn walk_dir(&self, dir: &PathBuf, scripts: &mut HashSet<String>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    self.walk_dir(&path, scripts);
                } else if path.extension().and_then(|s| s.to_str()) == Some("py") {
                    if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                        // Skip __init__.py and other dunder files
                        if !name.starts_with("__") {
                            scripts.insert(name.to_string());
                        }
                    }
                }
            }
        }
    }
}
