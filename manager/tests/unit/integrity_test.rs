use runpy::Manager;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn write_fake_uv(tmp: &TempDir, body: &str) -> PathBuf {
    let path = tmp.path().join("uv");
    fs::write(&path, body).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn scripts_dir(tmp: &TempDir) -> PathBuf {
    let scripts = tmp.path().join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    fs::write(scripts.join("hello.py"), "# stub").unwrap();
    scripts
}

fn manager_with_uv(scripts: &Path, uv: &Path) -> Manager {
    Manager::with_uv_path(scripts.to_str().unwrap(), uv.to_str().unwrap())
}

#[tokio::test]
async fn integrity_accepts_available_uv_and_scripts_directory() {
    let tmp = TempDir::new().unwrap();
    let uv = write_fake_uv(
        &tmp,
        "#!/bin/sh\n[ \"$1\" = \"--version\" ] && exit 0\nexit 1\n",
    );
    let scripts = scripts_dir(&tmp);
    let manager = manager_with_uv(&scripts, &uv);

    assert!(manager.check_integrity().is_ok());
}

#[tokio::test]
async fn integrity_rejects_missing_uv_executable() {
    let tmp = TempDir::new().unwrap();
    let scripts = scripts_dir(&tmp);
    let missing = tmp.path().join("missing-uv");
    let manager = manager_with_uv(&scripts, &missing);

    assert_eq!(
        manager.check_integrity(),
        Err(format!(
            "uv executable not found at '{}'",
            missing.display()
        ))
    );
}

#[tokio::test]
async fn integrity_rejects_uv_with_nonzero_status() {
    let tmp = TempDir::new().unwrap();
    let uv = write_fake_uv(&tmp, "#!/bin/sh\nexit 7\n");
    let scripts = scripts_dir(&tmp);
    let manager = manager_with_uv(&scripts, &uv);

    let error = manager.check_integrity().unwrap_err();
    assert!(error.contains("returned non-zero status"));
    assert!(error.contains(uv.to_str().unwrap()));
}

#[tokio::test]
async fn integrity_rejects_missing_scripts_directory() {
    let tmp = TempDir::new().unwrap();
    let uv = write_fake_uv(
        &tmp,
        "#!/bin/sh\n[ \"$1\" = \"--version\" ] && exit 0\nexit 1\n",
    );
    let missing = tmp.path().join("missing-scripts");
    let manager = manager_with_uv(&missing, &uv);

    assert_eq!(
        manager.check_integrity(),
        Err(format!(
            "Scripts directory does not exist: '{}'",
            missing.display()
        ))
    );
}
