//! Watchdog reports: sampled process-group resources and states.

use runpy::{Manager, ProcessState, WorkerReport};
use std::fs;
use std::time::{Duration, Instant};
use tempfile::TempDir;

use crate::manager_test::{scripts_dir, uv_script};

#[tokio::test]
async fn report_worker_returns_none_for_unknown_id() {
    let tmp = TempDir::new().unwrap();
    let manager = Manager::builder(tmp.path())
        .uv_path("true")
        .build()
        .await
        .unwrap();
    assert!(manager.watchdog().report_worker("missing").await.is_none());
}

/// Poll the worker's report until `check` holds.
async fn report_until(
    manager: &Manager,
    worker_id: &str,
    check: impl Fn(&WorkerReport) -> bool,
) -> WorkerReport {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let report = manager.watchdog().report_worker(worker_id).await.unwrap();
        if check(&report) {
            return report;
        }
        assert!(
            Instant::now() < deadline,
            "report never matched: {report:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

const MIB: u64 = 1024 * 1024;

/// Python snippet that touches `mib` MiB and then burns CPU.
fn hog(mib: u64) -> String {
    format!("python3 -c 'x = bytearray({mib} * 1024 * 1024)\nwhile True: pass'")
}

#[cfg(unix)]
#[tokio::test]
async fn reports_aggregate_memory_and_cpu_of_the_whole_group() {
    let tmp = TempDir::new().unwrap();
    // A Python descendant in the worker's group allocates and burns CPU.
    let uv = uv_script(
        &tmp,
        &format!("{} &\nexec python3 \"$FAKE_WORKER\" \"$@\"", hog(64)),
    );
    let scripts = scripts_dir(&tmp, &["hog"]);
    let manager = Manager::builder(&scripts)
        .uv_path(&uv)
        .build()
        .await
        .unwrap();
    let worker = manager.worker("hog").spawn().await.unwrap();

    // Poll until a window shows the busy loop: the first complete window may
    // still cover the descendant's startup.
    let report = report_until(&manager, worker.id(), |report| {
        report.cpu_percent.is_some_and(|cpu| cpu > 30.0)
            && report.memory_bytes.unwrap_or(0) >= 64 * MIB
    })
    .await;
    assert_eq!(report.state, ProcessState::Running);
    assert!(report.process_count.unwrap() >= 2, "{report:?}");
    assert!(report.sampled_at.is_some());
    worker.terminate().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn stopped_groups_are_reported_until_continued() {
    let tmp = TempDir::new().unwrap();
    let uv = uv_script(&tmp, "exec python3 \"$FAKE_WORKER\" \"$@\"");
    let scripts = scripts_dir(&tmp, &["worker"]);
    let manager = Manager::builder(&scripts)
        .uv_path(&uv)
        .build()
        .await
        .unwrap();
    let worker = manager.worker("worker").spawn().await.unwrap();
    let report = report_until(&manager, worker.id(), |report| {
        report.state == ProcessState::Running
    })
    .await;

    let group = -(report.pid as libc::pid_t);
    unsafe { libc::kill(group, libc::SIGSTOP) };
    report_until(&manager, worker.id(), |report| {
        report.state == ProcessState::Stopped
    })
    .await;
    unsafe { libc::kill(group, libc::SIGCONT) };
    report_until(&manager, worker.id(), |report| {
        report.state == ProcessState::Running
    })
    .await;
    worker.terminate().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn processes_that_leave_the_group_are_excluded() {
    let tmp = TempDir::new().unwrap();
    let escaped_pid = tmp.path().join("escaped.pid");
    // This descendant moves to its own process group before allocating.
    let escape = format!(
        "python3 -c 'import os, time; os.setpgid(0, 0); open(\"{}\", \"w\").write(str(os.getpid())); \
         x = bytearray(256 * 1024 * 1024); time.sleep(300)' &",
        escaped_pid.display()
    );
    let uv = uv_script(
        &tmp,
        &format!("{escape}\nexec python3 \"$FAKE_WORKER\" \"$@\""),
    );
    let scripts = scripts_dir(&tmp, &["worker"]);
    let manager = Manager::builder(&scripts)
        .uv_path(&uv)
        .build()
        .await
        .unwrap();
    let worker = manager.worker("worker").spawn().await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !escaped_pid.exists() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Let the escaped process allocate, then wait for two fresh samples.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let report = report_until(&manager, worker.id(), |report| {
        report.memory_bytes.is_some()
    })
    .await;
    assert!(
        report.memory_bytes.unwrap() < 200 * MIB,
        "escaped memory was counted: {report:?}"
    );

    let pid: libc::pid_t = fs::read_to_string(&escaped_pid).unwrap().parse().unwrap();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    worker.terminate().await.unwrap();
}
