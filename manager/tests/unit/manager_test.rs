use runpy::{
    InvalidScriptReason, Manager, RunpyError, StdinMode, TimeoutStage, WorkerOutputStream,
};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::task::{Context, Poll, Waker};
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

/// A fake uv that records `name=value` facts about its launch and then idles.
fn recording_uv(tmp: &TempDir, body: &str) -> PathBuf {
    write_executable(
        tmp,
        "uv",
        &format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then exit 0; fi\n{body}\n"),
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

async fn manager_with_uv(scripts: &Path, uv: &Path) -> Manager {
    Manager::builder(scripts).uv_path(uv).build().await.unwrap()
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
    let deadline = Instant::now() + Duration::from_secs(3);
    while process_exists(pid) {
        assert!(
            Instant::now() < deadline,
            "process {pid} survived worker termination"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn each_manager_owns_a_private_socket_directory_removed_on_drop() {
    let tmp = TempDir::new().unwrap();
    let uv = recording_uv(&tmp, "exec sleep 300");
    let scripts = scripts_dir(&tmp, &["same"]);
    let first = manager_with_uv(&scripts, &uv).await;
    let second = manager_with_uv(&scripts, &uv).await;
    assert_ne!(first.socket_dir(), second.socket_dir());

    let mode = fs::metadata(first.socket_dir())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);

    // The same script runs in both Managers without any socket conflict.
    first.worker("same").spawn().await.unwrap();
    second.worker("same").spawn().await.unwrap();
    for manager in [&first, &second] {
        let sockets: Vec<_> = fs::read_dir(manager.socket_dir()).unwrap().collect();
        assert_eq!(sockets.len(), 1);
    }

    let first_dir = first.socket_dir().to_path_buf();
    drop(first);
    assert!(!first_dir.exists());
    assert!(second.socket_dir().exists());
}

#[tokio::test]
async fn socket_parent_gets_a_fresh_child_and_is_never_adopted() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &[]);
    let parent = tmp.path().join("sockets");
    fs::create_dir(&parent).unwrap();
    fs::write(parent.join("existing"), "keep me").unwrap();

    let manager = Manager::builder(&scripts)
        .uv_path(&uv)
        .socket_parent(&parent)
        .build()
        .await
        .unwrap();
    let socket_dir = manager.socket_dir().to_path_buf();
    assert_eq!(socket_dir.parent().unwrap(), parent);
    drop(manager);

    assert!(!socket_dir.exists());
    assert_eq!(
        fs::read_to_string(parent.join("existing")).unwrap(),
        "keep me"
    );
}

#[tokio::test]
async fn unusable_socket_parents_are_rejected() {
    use std::os::unix::ffi::OsStrExt;

    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &[]);

    let long = tmp.path().join("p".repeat(120));
    fs::create_dir(&long).unwrap();
    assert!(matches!(
        Manager::builder(&scripts).uv_path(&uv).socket_parent(&long).build().await,
        Err(RunpyError::InvalidConfig(message)) if message.contains("too long")
    ));

    let non_utf8 = tmp.path().join(std::ffi::OsStr::from_bytes(b"bad-\xff"));
    fs::create_dir(&non_utf8).unwrap();
    assert!(matches!(
        Manager::builder(&scripts).uv_path(&uv).socket_parent(&non_utf8).build().await,
        Err(RunpyError::InvalidConfig(message)) if message.contains("UTF-8")
    ));

    assert!(matches!(
        Manager::builder(&scripts)
            .uv_path(&uv)
            .socket_parent(tmp.path().join("missing"))
            .build()
            .await,
        Err(RunpyError::Io(_))
    ));
}
#[test]
fn build_outside_a_tokio_runtime_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &[]);
    let mut build = Box::pin(Manager::builder(&scripts).uv_path(&uv).build());
    let mut context = Context::from_waker(Waker::noop());
    match build.as_mut().poll(&mut context) {
        Poll::Ready(Err(RunpyError::InvalidConfig(message))) => {
            assert!(message.contains("Tokio runtime"));
        }
        Poll::Ready(other) => panic!("unexpected build result: {:?}", other.err()),
        Poll::Pending => panic!("runtime detection must not wait"),
    }
}

#[tokio::test]
async fn invalid_configuration_is_rejected_before_checking_the_runtime() {
    let tmp = TempDir::new().unwrap();
    // A missing uv proves validation runs before any external check.
    let missing_uv = tmp.path().join("missing-uv");
    let scripts = scripts_dir(&tmp, &[]);
    let base = || Manager::builder(&scripts).uv_path(&missing_uv);

    for builder in [
        base().max_frame_bytes(0),
        base().max_frame_bytes(u32::MAX as usize),
        base().frame_timeout(Duration::ZERO),
        base().outbound_capacity(0),
        base().max_frame_bytes(1024).queued_bytes(1024),
        base().send_timeout(Duration::ZERO),
        base().shutdown_grace(Duration::ZERO),
    ] {
        let result = builder.build().await;
        assert!(
            matches!(result, Err(RunpyError::InvalidConfig(_))),
            "unexpected result: {:?}",
            result.err()
        );
    }
}

#[tokio::test]
async fn build_reports_uv_and_scripts_directory_failures_distinctly() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &[]);

    let missing_uv = tmp.path().join("missing-uv");
    match Manager::builder(&scripts)
        .uv_path(&missing_uv)
        .build()
        .await
    {
        Err(RunpyError::UvUnavailable { path, .. }) => assert_eq!(path, missing_uv),
        other => panic!("unexpected result: {:?}", other.err()),
    }

    let failing_uv = write_executable(&tmp, "failing-uv", "#!/bin/sh\nexit 7\n");
    assert!(matches!(
        Manager::builder(&scripts)
            .uv_path(&failing_uv)
            .build()
            .await,
        Err(RunpyError::UvUnavailable { .. })
    ));

    let missing_scripts = tmp.path().join("missing-scripts");
    match Manager::builder(&missing_scripts)
        .uv_path(&uv)
        .build()
        .await
    {
        Err(RunpyError::ScriptsDirectory { path, .. }) => assert_eq!(path, missing_scripts),
        other => panic!("unexpected result: {:?}", other.err()),
    }

    let file_scripts = tmp.path().join("not-a-directory");
    fs::write(&file_scripts, "").unwrap();
    assert!(matches!(
        Manager::builder(&file_scripts).uv_path(&uv).build().await,
        Err(RunpyError::ScriptsDirectory { .. })
    ));
}

#[tokio::test]
async fn worker_handles_stop_working_after_manager_drop() {
    let tmp = TempDir::new().unwrap();
    let uv = recording_uv(&tmp, "exec sleep 300");
    let scripts = scripts_dir(&tmp, &["test"]);
    let (builder, worker) = {
        let manager = manager_with_uv(&scripts, &uv).await;
        let worker = manager.worker("test").spawn().await.unwrap();
        (manager.worker("test"), worker)
    };

    assert!(matches!(
        builder.spawn().await,
        Err(RunpyError::ManagerClosed)
    ));
    assert!(matches!(
        worker.terminate().await,
        Err(RunpyError::ManagerClosed)
    ));
}

#[tokio::test]
async fn spawn_rejects_missing_worker_script_before_binding() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &[]);
    let manager = manager_with_uv(&scripts, &uv).await;

    match manager.worker("missing").spawn().await {
        Err(RunpyError::InvalidScript {
            script,
            reason: InvalidScriptReason::NotFound(path),
        }) => {
            assert_eq!(script, "missing");
            assert_eq!(path, scripts.canonicalize().unwrap().join("missing.py"));
        }
        other => panic!("unexpected result: {:?}", other.err()),
    }
}

#[tokio::test]
async fn spawn_rejects_names_that_are_not_single_path_components() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &["valid"]);
    fs::write(tmp.path().join("escape.py"), "# stub").unwrap();
    fs::create_dir(scripts.join("nested")).unwrap();
    fs::write(scripts.join("nested/worker.py"), "# stub").unwrap();
    let manager = manager_with_uv(&scripts, &uv).await;

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
        assert!(
            matches!(
                manager.worker(name).spawn().await,
                Err(RunpyError::InvalidScript {
                    reason: InvalidScriptReason::NotSingleComponent,
                    ..
                })
            ),
            "unexpected result for {name:?}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn stdin_defaults_to_null_and_cwd_can_be_overridden() {
    let tmp = TempDir::new().unwrap();
    let facts = tmp.path().join("facts");
    let work_dir = tmp.path().join("work");
    fs::create_dir(&work_dir).unwrap();
    let uv = recording_uv(
        &tmp,
        r#"cat > /dev/null
printf 'cwd=%s\npythonpath=%s\n' "$(pwd -P)" "$PYTHONPATH" > "$RUNPY_TEST_FACTS.tmp"
mv "$RUNPY_TEST_FACTS.tmp" "$RUNPY_TEST_FACTS"
exec sleep 300"#,
    );
    let scripts = scripts_dir(&tmp, &["managed"]);
    let manager = manager_with_uv(&scripts, &uv).await;
    let template = manager
        .worker("managed")
        .env("RUNPY_TEST_FACTS", facts.to_str().unwrap());

    // `cat` only finishes because stdin is the null device, not a terminal.
    let worker = template.clone().spawn().await.unwrap();
    wait_for_file(&facts).await;
    let scripts_parent = scripts.parent().unwrap().canonicalize().unwrap();
    assert_eq!(
        fs::read_to_string(&facts).unwrap(),
        format!(
            "cwd={}\npythonpath={}\n",
            scripts_parent.display(),
            scripts_parent.display()
        )
    );
    worker.terminate().await.ok();
    fs::remove_file(&facts).unwrap();

    let worker = template
        .cwd(&work_dir)
        .stdin(StdinMode::Null)
        .spawn()
        .await
        .unwrap();
    wait_for_file(&facts).await;
    assert!(fs::read_to_string(&facts).unwrap().starts_with(&format!(
        "cwd={}\n",
        work_dir.canonicalize().unwrap().display()
    )));
    worker.terminate().await.ok();
}

#[cfg(unix)]
#[tokio::test]
async fn worker_output_is_attributed_and_python_is_forced_unbuffered() {
    let tmp = TempDir::new().unwrap();
    let unbuffered_file = tmp.path().join("python-unbuffered");
    let uv = recording_uv(
        &tmp,
        r#"printf '%s' "$PYTHONUNBUFFERED" > "$RUNPY_TEST_UNBUFFERED"
printf 'startup stdout\n'
printf 'startup stderr\n' >&2
sleep 300"#,
    );
    let scripts = scripts_dir(&tmp, &["managed"]);
    let manager = manager_with_uv(&scripts, &uv).await;
    let (output_tx, output_rx) = std_mpsc::channel();
    manager.on_worker_output(move |output| {
        let _ = output_tx.send(output);
    });

    let worker = manager
        .worker("managed")
        .env("PYTHONUNBUFFERED", "0")
        .env("RUNPY_TEST_UNBUFFERED", unbuffered_file.to_str().unwrap())
        .spawn()
        .await
        .unwrap();
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
    assert_eq!(outputs[0].worker_id, worker.id());
    assert_eq!(outputs[0].stream, WorkerOutputStream::Stdout);
    assert_eq!(outputs[0].line, "startup stdout");
    assert_eq!(outputs[1].worker_id, worker.id());
    assert_eq!(outputs[1].stream, WorkerOutputStream::Stderr);
    assert_eq!(outputs[1].line, "startup stderr");
}

#[cfg(unix)]
#[tokio::test]
async fn high_volume_worker_output_does_not_block_process_progress() {
    let tmp = TempDir::new().unwrap();
    let marker_file = tmp.path().join("output-complete");
    let uv = recording_uv(
        &tmp,
        r#"dd if=/dev/zero bs=131072 count=1 2>/dev/null | tr '\0' x
dd if=/dev/zero bs=131072 count=1 2>/dev/null | tr '\0' y >&2
printf 'done' > "$RUNPY_TEST_OUTPUT_MARKER"
sleep 300"#,
    );
    let scripts = scripts_dir(&tmp, &["managed"]);
    let manager = manager_with_uv(&scripts, &uv).await;

    manager
        .worker("managed")
        .env("RUNPY_TEST_OUTPUT_MARKER", marker_file.to_str().unwrap())
        .spawn()
        .await
        .unwrap();
    wait_for_file(&marker_file).await;
    assert_eq!(fs::read_to_string(&marker_file).unwrap(), "done");
}

#[cfg(unix)]
#[tokio::test]
async fn uv_runner_receives_worker_arguments_and_termination_kills_process_group() {
    let tmp = TempDir::new().unwrap();
    let args_file = tmp.path().join("args");
    let descendant_file = tmp.path().join("descendant-pid");
    let uv = recording_uv(
        &tmp,
        r#"printf '%s\n' "$@" > "$RUNPY_TEST_ARGS"
sleep 300 &
descendant=$!
printf '%s' "$descendant" > "$RUNPY_TEST_DESCENDANT"
wait "$descendant""#,
    );
    let scripts = scripts_dir(&tmp, &["managed"]);
    let manager = manager_with_uv(&scripts, &uv).await;
    let worker = manager
        .worker("managed")
        .env("RUNPY_TEST_ARGS", args_file.to_str().unwrap())
        .env("RUNPY_TEST_DESCENDANT", descendant_file.to_str().unwrap())
        .arg("mode", "test")
        .spawn()
        .await
        .unwrap();
    wait_for_file(&args_file).await;
    wait_for_file(&descendant_file).await;

    let arguments: Vec<_> = fs::read_to_string(&args_file)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(&arguments[0..3], ["run", "--no-project", "--script"]);
    assert!(!arguments.iter().any(|argument| argument == "--locked"));
    assert_eq!(
        arguments[3],
        scripts
            .canonicalize()
            .unwrap()
            .join("managed.py")
            .to_str()
            .unwrap()
    );
    assert_eq!(
        Path::new(&arguments[4]).parent().unwrap(),
        manager.socket_dir()
    );
    assert_eq!(arguments[5], worker.id());
    assert_eq!(arguments[6], "--mode=test");

    let uv_pid = manager
        .watchdog()
        .report_worker(worker.id())
        .await
        .unwrap()
        .pid;
    let descendant_pid: u32 = fs::read_to_string(&descendant_file)
        .unwrap()
        .parse()
        .unwrap();
    assert!(process_exists(uv_pid));
    assert!(process_exists(descendant_pid));

    // The fake runner never connects, so the terminate frame cannot be
    // written; cleanup still stops the whole process group.
    assert!(matches!(
        worker.terminate().await,
        Err(RunpyError::Timeout(TimeoutStage::Queued))
    ));
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
    let uv = recording_uv(&tmp, r#"printf '%s\n' "$@" > "$RUNPY_TEST_ARGS""#);
    let scripts = scripts_dir(&tmp, &["managed"]);
    fs::write(scripts.join("managed.py.lock"), "locked").unwrap();

    let relative_uv = uv.strip_prefix(&current_dir).unwrap();
    let relative_scripts = scripts.strip_prefix(&current_dir).unwrap();
    let manager = manager_with_uv(relative_scripts, relative_uv).await;
    let worker = manager
        .worker("managed")
        .env("RUNPY_TEST_ARGS", args_file.to_str().unwrap())
        .spawn()
        .await
        .unwrap();
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
    assert_eq!(
        Path::new(&arguments[5]).parent().unwrap(),
        manager.socket_dir()
    );
    assert_eq!(arguments[6], worker.id());
}

#[cfg(unix)]
#[tokio::test]
async fn watchdog_cleanup_kills_descendants_after_uv_exits() {
    let tmp = TempDir::new().unwrap();
    let descendant_file = tmp.path().join("descendant-pid");
    let release_file = tmp.path().join("release-uv");
    let uv = recording_uv(
        &tmp,
        r#"sleep 300 &
printf '%s' "$!" > "$RUNPY_TEST_DESCENDANT"
while [ ! -e "$RUNPY_TEST_RELEASE" ]; do sleep 0.01; done
exit 0"#,
    );
    let scripts = scripts_dir(&tmp, &["managed"]);
    let manager = manager_with_uv(&scripts, &uv).await;
    let worker = manager
        .worker("managed")
        .env("RUNPY_TEST_DESCENDANT", descendant_file.to_str().unwrap())
        .env("RUNPY_TEST_RELEASE", release_file.to_str().unwrap())
        .spawn()
        .await
        .unwrap();
    let socket_path = fs::read_dir(manager.socket_dir())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    wait_for_file(&descendant_file).await;
    let descendant_pid: u32 = fs::read_to_string(&descendant_file)
        .unwrap()
        .parse()
        .unwrap();
    let uv_pid = manager
        .watchdog()
        .report_worker(worker.id())
        .await
        .unwrap()
        .pid;
    assert!(process_exists(uv_pid));
    assert!(process_exists(descendant_pid));
    assert!(socket_path.exists());

    fs::write(&release_file, "go").unwrap();
    let deadline = Instant::now() + Duration::from_secs(7);
    loop {
        let worker_removed = manager
            .watchdog()
            .report_worker(worker.id())
            .await
            .is_none();
        if worker_removed && process_is_stopped(descendant_pid) && !socket_path.exists() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "watchdog did not remove worker and stop descendant process"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn one_watchdog_reports_workers_from_the_shared_control_plane() {
    let tmp = TempDir::new().unwrap();
    let uv = recording_uv(&tmp, "exec sleep 300");
    let scripts = scripts_dir(&tmp, &["first", "second"]);
    let manager = manager_with_uv(&scripts, &uv).await;
    let first = manager.worker("first").spawn().await.unwrap();
    let second = manager.worker("second").spawn().await.unwrap();

    let mut reported: Vec<_> = manager
        .watchdog()
        .report()
        .await
        .into_iter()
        .map(|report| report.worker_name)
        .collect();
    let mut expected = vec![first.id().to_string(), second.id().to_string()];
    reported.sort();
    expected.sort();
    assert_eq!(reported, expected);
}

#[tokio::test]
async fn watchdog_report_is_empty_without_workers() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &[]);
    let manager = manager_with_uv(&scripts, &uv).await;

    assert!(manager.watchdog().report().await.is_empty());
}
