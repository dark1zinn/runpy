use runpy::{Envelope, Manager, Worker, WorkerIdentity};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn write_executable(tmp: &TempDir, name: &str, body: &str) -> PathBuf {
    let path = tmp.path().join(name);
    fs::write(&path, body).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn fake_uv(tmp: &TempDir) -> PathBuf {
    write_executable(
        tmp,
        "uv",
        "#!/bin/sh\n[ \"$1\" = \"--version\" ] && exit 0\nexit 1\n",
    )
}

fn scripts_dir(tmp: &TempDir, names: &[&str]) -> PathBuf {
    let scripts = tmp.path().join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    for name in names {
        fs::write(scripts.join(format!("{name}.py")), "# stub").unwrap();
    }
    scripts
}

fn manager_with_uv(scripts: &Path, uv: &Path) -> Manager {
    Manager::with_uv_path(scripts.to_str().unwrap(), uv.to_str().unwrap())
}

async fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
async fn wait_for_process_exit(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_exists(pid) {
        assert!(
            Instant::now() < deadline,
            "process {pid} survived worker termination"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn worker_identity_contains_script_name() {
    let id = WorkerIdentity::new("my_script");
    assert!(id.name.starts_with("my_script_"));
    assert!(id.sock_file.starts_with("rp_"));
    assert!(id.sock_file.ends_with(".sock"));
}

#[test]
fn worker_identity_is_unique() {
    let first = WorkerIdentity::new("same");
    let second = WorkerIdentity::new("same");
    assert_ne!(first.name, second.name);
    assert_ne!(first.sock_file, second.sock_file);
}

#[tokio::test]
async fn manager_accepts_uv_and_scripts_paths() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &["hello"]);
    let manager = manager_with_uv(&scripts, &uv);

    assert!(manager.check_integrity().is_ok());
}

#[tokio::test]
async fn worker_builder_methods_are_chainable() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &["test"]);
    let manager = manager_with_uv(&scripts, &uv);
    let mut worker: Worker = manager.worker("test");

    worker
        .env("A", "1")
        .env("B", "2")
        .arg("mode", "test")
        .on_message(|_| {});
}

#[tokio::test]
async fn send_and_terminate_before_spawn_return_errors() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &["test"]);
    let manager = manager_with_uv(&scripts, &uv);
    let worker = manager.worker("test");

    let send_error = worker
        .send_message(Envelope::terminate())
        .await
        .unwrap_err();
    assert!(send_error.contains("not been spawned"));

    let terminate_error = worker.terminate().await.unwrap_err();
    assert!(terminate_error.contains("not been spawned"));
}

#[tokio::test]
async fn spawn_rejects_missing_worker_script_before_binding() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &[]);
    let manager = manager_with_uv(&scripts, &uv);
    let mut worker = manager.worker("missing");

    assert_eq!(
        worker.spawn().await,
        Err(format!(
            "Worker script does not exist: '{}'",
            scripts.join("missing.py").display()
        ))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn uv_runner_receives_worker_arguments_and_termination_kills_process_group() {
    let tmp = TempDir::new().unwrap();
    let args_file = tmp.path().join("args");
    let descendant_file = tmp.path().join("descendant-pid");
    let uv = write_executable(
        &tmp,
        "uv",
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
    echo "uv 0.11.2"
    exit 0
fi
printf '%s\n' "$@" > "$RUNPY_TEST_ARGS"
sleep 300 &
descendant=$!
printf '%s' "$descendant" > "$RUNPY_TEST_DESCENDANT"
wait "$descendant"
"#,
    );
    let scripts = scripts_dir(&tmp, &["managed"]);
    let manager = manager_with_uv(&scripts, &uv);
    let mut worker = manager.worker("managed");
    worker
        .env("RUNPY_TEST_ARGS", args_file.to_str().unwrap())
        .env("RUNPY_TEST_DESCENDANT", descendant_file.to_str().unwrap())
        .arg("mode", "test");

    let worker_id = worker.spawn().await.unwrap();
    wait_for_file(&args_file).await;
    wait_for_file(&descendant_file).await;

    let arguments: Vec<_> = fs::read_to_string(&args_file)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(&arguments[0..3], ["run", "--no-project", "--script"]);
    assert_eq!(arguments[3], scripts.join("managed.py").to_str().unwrap());
    assert!(arguments[4].starts_with("/tmp/runpy/rp_managed_"));
    assert_eq!(arguments[5], worker_id);
    assert_eq!(arguments[6], "--mode=test");

    let uv_pid = manager.dog.report_worker(&worker_id).await.unwrap().pid;
    let descendant_pid: u32 = fs::read_to_string(&descendant_file)
        .unwrap()
        .parse()
        .unwrap();
    assert!(process_exists(uv_pid));
    assert!(process_exists(descendant_pid));

    worker.terminate().await.unwrap();
    wait_for_process_exit(uv_pid).await;
    wait_for_process_exit(descendant_pid).await;
}

#[tokio::test]
async fn manager_drop_does_not_panic_without_workers() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &[]);
    let manager = manager_with_uv(&scripts, &uv);

    drop(manager);
}

#[tokio::test]
async fn watchdog_report_is_empty_without_workers() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &[]);
    let manager = manager_with_uv(&scripts, &uv);

    assert!(manager.dog.report().await.is_empty());
}
