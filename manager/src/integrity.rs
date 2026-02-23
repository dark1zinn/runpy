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

    pub fn perform_check(&self) -> Result<(), String> {
        // Validate Venv
        if !self.validate_venv() {
            panic!("Python executable missing in .venv");
            // return Err("Python executable missing in .venv".into());
        }

        let sock_dir = PathBuf::from("/tmp/runpy");
        if !sock_dir.exists() {
            std::fs::create_dir_all(&sock_dir).map_err(|e| format!("Failed to create socket directory: {}", e))?;
        }

        // Index Scripts
        self.index_scripts();

        Ok(())
    }
    
    pub fn check_script(&self, script: &str) -> bool {
        
        // Refresh registry before check
        self.index_scripts();
        let scripts = self.registry.lock().unwrap();
        scripts.contains(script)
    }

    fn validate_venv(&self) -> bool {
        let py_bin = if cfg!(windows) { "Scripts/python.exe" } else { "bin/python" };
        self.venv_path.join(py_bin).exists()
    }

    fn index_scripts(&self) {
        let mut scripts = self.registry.lock().unwrap();
        scripts.clear();
        if let Ok(entries) = std::fs::read_dir(&self.scripts_dir) {
            for entry in entries.flatten() {
                if entry.path().extension().and_then(|s| s.to_str()) == Some("py") {
                    if let Some(name) = entry.path().file_stem().and_then(|s| s.to_str()) {
                        scripts.insert(name.to_string());
                    }
                }
            }
        }
        println!("Indexed scripts: {:?} \n  - {:?}", scripts.len(), scripts);
    }
}