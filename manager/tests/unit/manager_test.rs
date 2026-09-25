use runpy::{Envelope, Manager, Worker, WorkerIdentity, WorkerOutputStream};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
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

#[cfg(target_os = "linux")]
fn process_is_stopped(pid: u32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return !process_exists(pid);
    };
    let Some((_, fields)) = stat.rsplit_once(") ") else {
        return false;
    };

    matches!(fields.as_bytes().first().copied(), Some(b'Z' | b'X'))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_is_stopped(pid: u32) -> bool {
    !process_exists(pid)
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

#[test]
fn manager_constructors_work_without_a_tokio_runtime() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &[]);

    drop(manager_with_uv(&scripts, &uv));
    drop(Manager::new(scripts.to_str().unwrap()));
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
async fn manager_returns_one_shared_logger() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &[]);
    let manager = manager_with_uv(&scripts, &uv);

    assert!(Arc::ptr_eq(&manager.logger(), &manager.logger()));
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
async fn worker_facade_rejects_spawn_after_manager_drop() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &["test"]);
    let mut worker = {
        let manager = manager_with_uv(&scripts, &uv);
        manager.worker("test")
    };

    assert_eq!(
        worker.spawn().await,
        Err("Worker manager is no longer available".to_string())
    );
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

#[tokio::test]
async fn spawn_rejects_names_that_are_not_single_path_components() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &["valid"]);
    fs::write(tmp.path().join("escape.py"), "# stub").unwrap();
    fs::create_dir(scripts.join("nested")).unwrap();
    fs::write(scripts.join("nested/worker.py"), "# stub").unwrap();
    let manager = manager_with_uv(&scripts, &uv);

    for name in [
        "../escape",
        "nested/worker",
        "./valid",
        "valid/",
        ".",
        "..",
        "",
        "/absolute",
    ] {
        let mut worker = manager.worker(name);
        assert_eq!(
            worker.spawn().await,
            Err("Worker script name must be a single path component".to_string()),
            "unexpected result for {name:?}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn worker_output_is_attributed_and_python_is_forced_unbuffered() {
    let tmp = TempDir::new().unwrap();
    let unbuffered_file = tmp.path().join("python-unbuffered");
    let uv = write_executable(
        &tmp,
        "uv",
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
    exit 0
fi
printf '%s' "$PYTHONUNBUFFERED" > "$RUNPY_TEST_UNBUFFERED"
printf 'startup stdout\n'
printf 'startup stderr\n' >&2
sleep 300
"#,
    );
    let scripts = scripts_dir(&tmp, &["managed"]);
    let mut manager = manager_with_uv(&scripts, &uv);
    let (output_tx, output_rx) = std_mpsc::channel();
    manager.on_worker_output(move |output| {
        let _ = output_tx.send(output);
    });
    let mut worker = manager.worker("managed");
    worker
        .env("PYTHONUNBUFFERED", "0")
        .env("RUNPY_TEST_UNBUFFERED", unbuffered_file.to_str().unwrap());

    let worker_id = worker.spawn().await.unwrap();
    wait_for_file(&unbuffered_file).await;
    assert_eq!(fs::read_to_string(&unbuffered_file).unwrap(), "1");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut outputs = Vec::new();
    while outputs.len() < 2 {
        while let Ok(output) = output_rx.try_recv() {
            outputs.push(output);
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for attributed worker output"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    outputs.sort_by_key(|output| match output.stream {
        WorkerOutputStream::Stdout => 0,
        WorkerOutputStream::Stderr => 1,
    });
    assert_eq!(outputs[0].worker_id, worker_id);
    assert_eq!(outputs[0].stream, WorkerOutputStream::Stdout);
    assert_eq!(outputs[0].line, "startup stdout");
    assert_eq!(outputs[1].worker_id, worker_id);
    assert_eq!(outputs[1].stream, WorkerOutputStream::Stderr);
    assert_eq!(outputs[1].line, "startup stderr");

    drop(manager);
}

#[cfg(unix)]
#[tokio::test]
async fn high_volume_worker_output_does_not_block_process_progress() {
    let tmp = TempDir::new().unwrap();
    let marker_file = tmp.path().join("output-complete");
    let uv = write_executable(
        &tmp,
        "uv",
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
    exit 0
fi
dd if=/dev/zero bs=131072 count=1 2>/dev/null | tr '\0' x
dd if=/dev/zero bs=131072 count=1 2>/dev/null | tr '\0' y >&2
printf 'done' > "$RUNPY_TEST_OUTPUT_MARKER"
sleep 300
"#,
    );
    let scripts = scripts_dir(&tmp, &["managed"]);
    let manager = manager_with_uv(&scripts, &uv);
    let mut worker = manager.worker("managed");
    worker.env("RUNPY_TEST_OUTPUT_MARKER", marker_file.to_str().unwrap());

    worker.spawn().await.unwrap();
    wait_for_file(&marker_file).await;
    assert_eq!(fs::read_to_string(&marker_file).unwrap(), "done");

    drop(manager);
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
    assert!(!arguments.iter().any(|argument| argument == "--locked"));
    assert_eq!(arguments[3], scripts.join("managed.py").to_str().unwrap());
    assert!(arguments[4].starts_with("/tmp/runpy/rp_managed_"));
    assert_eq!(arguments[5], worker_id);
    assert_eq!(arguments[6], "--mode=test");

    let uv_pid = manager
        .watchdog()
        .report_worker(&worker_id)
        .await
        .unwrap()
        .pid;
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

#[cfg(unix)]
#[tokio::test]
async fn relative_runtime_paths_are_absolutized_and_adjacent_lock_is_enforced() {
    let current_dir = std::env::current_dir().unwrap();
    let tmp = tempfile::Builder::new()
        .prefix(".runpy-relative-")
        .tempdir_in(&current_dir)
        .unwrap();
    let args_file = tmp.path().join("args");
    let uv = write_executable(
        &tmp,
        "uv",
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
    echo "uv 0.11.2"
    exit 0
fi
printf '%s\n' "$@" > "$RUNPY_TEST_ARGS"
"#,
    );
    let scripts = scripts_dir(&tmp, &["managed"]);
    fs::write(scripts.join("managed.py.lock"), "locked").unwrap();

    let relative_uv = uv.strip_prefix(&current_dir).unwrap();
    let relative_scripts = scripts.strip_prefix(&current_dir).unwrap();
    let manager = manager_with_uv(relative_scripts, relative_uv);
    let mut worker = manager.worker("managed");
    worker.env("RUNPY_TEST_ARGS", args_file.to_str().unwrap());

    let worker_id = worker.spawn().await.unwrap();
    wait_for_file(&args_file).await;

    let arguments: Vec<_> = fs::read_to_string(&args_file)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        &arguments[0..4],
        ["run", "--no-project", "--locked", "--script"]
    );
    assert_eq!(
        arguments[4],
        scripts
            .canonicalize()
            .unwrap()
            .join("managed.py")
            .to_str()
            .unwrap()
    );
    assert!(arguments[5].starts_with("/tmp/runpy/rp_managed_"));
    assert_eq!(arguments[6], worker_id);
}

#[cfg(unix)]
#[tokio::test]
async fn watchdog_cleanup_kills_descendants_after_uv_exits() {
    let tmp = TempDir::new().unwrap();
    let descendant_file = tmp.path().join("descendant-pid");
    let release_file = tmp.path().join("release-uv");
    let uv = write_executable(
        &tmp,
        "uv",
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
    echo "uv 0.11.2"
    exit 0
fi
sleep 300 &
printf '%s' "$!" > "$RUNPY_TEST_DESCENDANT"
while [ ! -e "$RUNPY_TEST_RELEASE" ]; do sleep 0.01; done
exit 0
"#,
    );
    let scripts = scripts_dir(&tmp, &["managed"]);
    let manager = manager_with_uv(&scripts, &uv);
    let mut worker = manager.worker("managed");
    worker
        .env("RUNPY_TEST_DESCENDANT", descendant_file.to_str().unwrap())
        .env("RUNPY_TEST_RELEASE", release_file.to_str().unwrap());

    let worker_id = worker.spawn().await.unwrap();
    let socket_path = PathBuf::from(format!("/tmp/runpy/rp_{worker_id}.sock"));
    wait_for_file(&descendant_file).await;
    let descendant_pid: u32 = fs::read_to_string(&descendant_file)
        .unwrap()
        .parse()
        .unwrap();
    let uv_pid = manager
        .watchdog()
        .report_worker(&worker_id)
        .await
        .unwrap()
        .pid;
    assert!(process_exists(uv_pid));
    assert!(process_exists(descendant_pid));
    assert!(socket_path.exists());

    fs::write(&release_file, "go").unwrap();
    let deadline = Instant::now() + Duration::from_secs(7);
    loop {
        let worker_removed = manager.watchdog().report_worker(&worker_id).await.is_none();
        if worker_removed && process_is_stopped(descendant_pid) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "watchdog did not remove worker and stop descendant process"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(!socket_path.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn one_watchdog_reports_workers_from_the_shared_control_plane() {
    let tmp = TempDir::new().unwrap();
    let uv = write_executable(
        &tmp,
        "uv",
        "#!/bin/sh\n[ \"$1\" = \"--version\" ] && exit 0\nexec sleep 300\n",
    );
    let scripts = scripts_dir(&tmp, &["first", "second"]);
    let manager = manager_with_uv(&scripts, &uv);
    let mut first = manager.worker("first");
    let mut second = manager.worker("second");
    let first_id = first.spawn().await.unwrap();
    let second_id = second.spawn().await.unwrap();

    let mut reported: Vec<_> = manager
        .watchdog()
        .report()
        .await
        .into_iter()
        .map(|report| report.worker_name)
        .collect();
    let mut expected = vec![first_id, second_id];
    reported.sort();
    expected.sort();
    assert_eq!(reported, expected);

    drop(manager);
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

    assert!(manager.watchdog().report().await.is_empty());
}
