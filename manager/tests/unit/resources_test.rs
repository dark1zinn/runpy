//! Resource lifetimes under churn.
//!
//! Each scenario runs in a child process of this test binary, so its file
//! descriptor and child-process accounting is not disturbed by tests running
//! concurrently in the parent.

use crate::manager_test::{scripts_dir, uv_script};
use runpy::{Manager, PoolConfig, RunpyError};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const CHILD: &str = "RUNPY_RESOURCE_CHILD";
const CYCLES: usize = 100;
const TIMEOUT: Duration = Duration::from_secs(10);

fn run_isolated(test: &str) {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            test,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "isolated {test} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn open_fds() -> usize {
    let directory = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    std::fs::read_dir(directory).unwrap().count()
}

/// True when this process has no child, running or unreaped.
fn no_children() -> bool {
    let mut status = 0;
    let result = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
    result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
}

fn process_exists(pid: i64) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Wait until every resource the Manager acquired is released again.
async fn assert_released(manager: &Manager, baseline_fds: usize) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let sockets = std::fs::read_dir(manager.socket_dir()).unwrap().count();
        let report = manager.watchdog().report().await;
        let fds = open_fds();
        if sockets == 0 && report.is_empty() && fds <= baseline_fds && no_children() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "resources leaked: {sockets} sockets, {} workers, {fds} fds (baseline {baseline_fds}), children: {}",
            report.len(),
            !no_children()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Every Manager-wide slot is free again: a full set spawns at once.
    let workers = spawn_all(manager, runpy::DEFAULT_MAX_WORKERS).await;
    for worker in workers {
        worker.terminate().await.unwrap();
    }
}

async fn spawn_all(manager: &Manager, count: usize) -> Vec<runpy::Worker> {
    let mut workers = Vec::new();
    for _ in 0..count {
        workers.push(
            tokio::time::timeout(Duration::from_secs(2), manager.worker("churn").spawn())
                .await
                .expect("slot was not returned")
                .unwrap(),
        );
    }
    workers
}

async fn churn_manager(tmp: &TempDir) -> Manager {
    let uv = uv_script(tmp, "exec python3 \"$FAKE_WORKER\" \"$@\"");
    let scripts = scripts_dir(tmp, &["churn"]);
    Manager::builder(&scripts)
        .uv_path(&uv)
        .build()
        .await
        .unwrap()
}

fn data(value: Value) -> runpy::Data {
    value.as_object().cloned().unwrap()
}

#[test]
fn spawn_churn_returns_every_resource() {
    run_isolated("resources_test::spawn_churn_child");
}

#[test]
fn pool_churn_returns_every_resource() {
    run_isolated("resources_test::pool_churn_child");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs isolated through spawn_churn_returns_every_resource"]
async fn spawn_churn_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let manager = churn_manager(&tmp).await;
    // Warm up lazily created runtime resources before taking the baseline.
    let warm = manager.worker("churn").spawn().await.unwrap();
    warm.execute(data(json!({})), TIMEOUT).await.unwrap();
    warm.terminate().await.unwrap();
    assert_released(&manager, usize::MAX).await;
    let baseline = open_fds();

    for cycle in 0..CYCLES {
        let worker = manager.worker("churn").spawn().await.unwrap();
        let pid = worker.execute(data(json!({})), TIMEOUT).await.unwrap()["pid"]
            .as_i64()
            .unwrap();
        if cycle % 10 == 9 {
            // Abrupt death mid-request instead of an orderly shutdown.
            assert!(matches!(
                worker.execute(data(json!({"exit": true})), TIMEOUT).await,
                Err(RunpyError::OutcomeUnknown { .. })
            ));
            worker.wait().await.unwrap();
        } else {
            worker.terminate().await.unwrap();
        }
        assert!(!process_exists(pid), "cycle {cycle}: worker {pid} survived");
    }
    assert_released(&manager, baseline).await;

    let socket_dir = manager.socket_dir().to_path_buf();
    drop(manager);
    assert!(!Path::new(&socket_dir).exists());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs isolated through pool_churn_returns_every_resource"]
async fn pool_churn_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let manager = churn_manager(&tmp).await;
    let warm = manager.worker("churn").spawn().await.unwrap();
    warm.terminate().await.unwrap();
    assert_released(&manager, usize::MAX).await;
    let baseline = open_fds();

    for round in 0..5 {
        // Alternate warm reusable pools with retire-after-request pools.
        let reuse = round % 2 == 0;
        let config = PoolConfig::default()
            .min_workers(usize::from(reuse))
            .reuse_workers(reuse)
            .idle_timeout(Duration::from_millis(50));
        let pool = manager.pool(manager.worker("churn"), config).await.unwrap();
        let mut requests = Vec::new();
        for _ in 0..CYCLES / 5 {
            requests.push(pool.submit(data(json!({})), TIMEOUT).await.unwrap());
        }
        for request in requests {
            request.result().await.unwrap();
        }
        pool.close().await;
    }
    assert_released(&manager, baseline).await;
}
