use runpy::{Manager, RunpyError};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
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

#[tokio::test]
async fn check_integrity_reports_runtime_changes_after_build() {
    let tmp = TempDir::new().unwrap();
    let uv = write_fake_uv(
        &tmp,
        "#!/bin/sh\n[ \"$1\" = \"--version\" ] && exit 0\nexit 1\n",
    );
    let scripts = scripts_dir(&tmp);
    let manager = Manager::builder(&scripts)
        .uv_path(&uv)
        .build()
        .await
        .unwrap();
    manager.check_integrity().await.unwrap();

    fs::write(&uv, "#!/bin/sh\nexit 7\n").unwrap();
    assert!(matches!(
        manager.check_integrity().await,
        Err(RunpyError::UvUnavailable { .. })
    ));

    fs::write(&uv, "#!/bin/sh\nexit 0\n").unwrap();
    fs::remove_dir_all(&scripts).unwrap();
    match manager.check_integrity().await {
        Err(RunpyError::ScriptsDirectory { path, .. }) => assert_eq!(path, scripts),
        other => panic!("unexpected result: {:?}", other.err()),
    }
}
