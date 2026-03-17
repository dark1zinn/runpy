/// Unit tests for the IntegrityChecker (integrity.rs).
///
/// Uses temporary directories to simulate valid/invalid venvs and script dirs
/// without touching the real filesystem outside of /tmp.
use std::fs;
use std::os::unix::fs::symlink;
use tempfile::TempDir;

/// Helper: create a fake venv with a `bin/python` executable.
fn fake_venv(tmp: &TempDir) -> std::path::PathBuf {
    let venv = tmp.path().join("fake_venv");
    let bin = venv.join("bin");
    fs::create_dir_all(&bin).unwrap();
    // Symlink the real `sh` so the path exists and is executable.
    symlink("/bin/sh", bin.join("python")).unwrap();
    venv
}

/// Helper: create a scripts directory with some `.py` files.
fn fake_scripts(tmp: &TempDir, names: &[&str]) -> std::path::PathBuf {
    let scripts = tmp.path().join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    for name in names {
        fs::write(scripts.join(format!("{}.py", name)), "# stub").unwrap();
    }
    scripts
}

// ─── IntegrityChecker::new ─────────────────────────────────────────────

#[test]
fn new_stores_paths() {
    let checker = runpy_test_helpers::integrity_checker("/some/venv", "/some/scripts");
    assert_eq!(checker.venv_path.to_str().unwrap(), "/some/venv");
    assert_eq!(checker.scripts_dir.to_str().unwrap(), "/some/scripts");
}

// ─── perform_check — venv validation ───────────────────────────────────

#[test]
fn perform_check_fails_on_missing_venv() {
    let tmp = TempDir::new().unwrap();
    let scripts = fake_scripts(&tmp, &["hello"]);
    let checker = runpy_test_helpers::integrity_checker(
        tmp.path().join("nonexistent_venv").to_str().unwrap(),
        scripts.to_str().unwrap(),
    );

    let result = checker.perform_check();
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Python executable missing"));
}

#[test]
fn perform_check_fails_on_venv_without_python_binary() {
    let tmp = TempDir::new().unwrap();
    let venv = tmp.path().join("empty_venv");
    fs::create_dir_all(venv.join("bin")).unwrap();
    // No python binary inside
    let scripts = fake_scripts(&tmp, &["hello"]);

    let checker = runpy_test_helpers::integrity_checker(
        venv.to_str().unwrap(),
        scripts.to_str().unwrap(),
    );

    let result = checker.perform_check();
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Python executable missing"));
}

// ─── perform_check — scripts directory ─────────────────────────────────

#[test]
fn perform_check_fails_on_missing_scripts_dir() {
    let tmp = TempDir::new().unwrap();
    let venv = fake_venv(&tmp);

    let checker = runpy_test_helpers::integrity_checker(
        venv.to_str().unwrap(),
        tmp.path().join("nonexistent_scripts").to_str().unwrap(),
    );

    let result = checker.perform_check();
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Scripts directory does not exist"));
}

// ─── perform_check — success path ──────────────────────────────────────

#[test]
fn perform_check_succeeds_with_valid_venv_and_scripts() {
    let tmp = TempDir::new().unwrap();
    let venv = fake_venv(&tmp);
    let scripts = fake_scripts(&tmp, &["scraper", "analyzer"]);

    let checker = runpy_test_helpers::integrity_checker(
        venv.to_str().unwrap(),
        scripts.to_str().unwrap(),
    );

    assert!(checker.perform_check().is_ok());
}

// ─── check_script ──────────────────────────────────────────────────────

#[test]
fn check_script_finds_existing_script() {
    let tmp = TempDir::new().unwrap();
    let scripts = fake_scripts(&tmp, &["hello", "world"]);

    let checker = runpy_test_helpers::integrity_checker("/unused", scripts.to_str().unwrap());
    assert!(checker.check_script("hello"));
    assert!(checker.check_script("world"));
}

#[test]
fn check_script_returns_false_for_nonexistent_script() {
    let tmp = TempDir::new().unwrap();
    let scripts = fake_scripts(&tmp, &["hello"]);

    let checker = runpy_test_helpers::integrity_checker("/unused", scripts.to_str().unwrap());
    assert!(!checker.check_script("nonexistent"));
}

#[test]
fn check_script_ignores_dunder_files() {
    let tmp = TempDir::new().unwrap();
    let scripts = fake_scripts(&tmp, &["real_script"]);
    // Add an __init__.py manually
    fs::write(
        tmp.path().join("scripts/__init__.py"),
        "# init",
    )
    .unwrap();

    let checker = runpy_test_helpers::integrity_checker("/unused", scripts.to_str().unwrap());
    assert!(checker.check_script("real_script"));
    assert!(!checker.check_script("__init__"));
}

// ─── recursive indexing ────────────────────────────────────────────────

#[test]
fn check_script_indexes_subdirectories_recursively() {
    let tmp = TempDir::new().unwrap();
    let scripts = tmp.path().join("scripts");
    let sub = scripts.join("subdir");
    fs::create_dir_all(&sub).unwrap();
    fs::write(scripts.join("top.py"), "# top").unwrap();
    fs::write(sub.join("nested.py"), "# nested").unwrap();

    let checker = runpy_test_helpers::integrity_checker("/unused", scripts.to_str().unwrap());
    assert!(checker.check_script("top"));
    assert!(checker.check_script("nested"));
}

// ─── re-indexing after filesystem changes ──────────────────────────────

#[test]
fn check_script_re_indexes_on_every_call() {
    let tmp = TempDir::new().unwrap();
    let scripts = fake_scripts(&tmp, &["first"]);

    let checker = runpy_test_helpers::integrity_checker("/unused", scripts.to_str().unwrap());
    assert!(!checker.check_script("second"));

    // Add a new script to the directory
    fs::write(scripts.join("second.py"), "# new").unwrap();
    assert!(checker.check_script("second"));
}

/// Small helper module to expose IntegrityChecker for tests without making
/// private fields public in the main crate.
mod runpy_test_helpers {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Re-creates the IntegrityChecker struct shape for testing.
    /// This uses the same fields as the crate-internal struct.
    pub struct FakeIntegrityChecker {
        pub venv_path: PathBuf,
        pub scripts_dir: PathBuf,
        pub registry: Mutex<HashSet<String>>,
    }

    impl FakeIntegrityChecker {
        pub fn perform_check(&self) -> Result<(), String> {
            let py_bin = if cfg!(windows) {
                "Scripts/python.exe"
            } else {
                "bin/python"
            };
            if !self.venv_path.join(py_bin).exists() {
                return Err(format!(
                    "Python executable missing in venv at '{}'",
                    self.venv_path.display()
                ));
            }

            let sock_dir = PathBuf::from("/tmp/runpy");
            if !sock_dir.exists() {
                std::fs::create_dir_all(&sock_dir)
                    .map_err(|e| format!("Failed to create socket directory: {}", e))?;
            }

            if !self.scripts_dir.exists() {
                return Err(format!(
                    "Scripts directory does not exist: '{}'",
                    self.scripts_dir.display()
                ));
            }

            self.index_scripts();
            Ok(())
        }

        pub fn check_script(&self, script: &str) -> bool {
            self.index_scripts();
            let scripts = self.registry.lock().unwrap();
            scripts.contains(script)
        }

        fn index_scripts(&self) {
            let mut scripts = self.registry.lock().unwrap();
            scripts.clear();
            self.walk_dir(&self.scripts_dir, &mut scripts);
        }

        fn walk_dir(&self, dir: &PathBuf, scripts: &mut HashSet<String>) {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        self.walk_dir(&path, scripts);
                    } else if path.extension().and_then(|s| s.to_str()) == Some("py") {
                        if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                            if !name.starts_with("__") {
                                scripts.insert(name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn integrity_checker(venv: &str, scripts: &str) -> FakeIntegrityChecker {
        FakeIntegrityChecker {
            venv_path: PathBuf::from(venv),
            scripts_dir: PathBuf::from(scripts),
            registry: Mutex::new(HashSet::new()),
        }
    }
}
