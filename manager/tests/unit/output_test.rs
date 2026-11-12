//! Whether worker output reaches the host's own log.
//!
//! Mirrored output is written to the host process's stderr, so each scenario
//! runs in a child process of this test binary whose stderr is inspected.

use crate::manager_test::{scripts_dir, uv_script};
use runpy::Manager;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;
use tempfile::TempDir;

const CHILD: &str = "RUNPY_OUTPUT_CHILD";
const SECRET: &str = "API_TOKEN=not-a-real-secret";

/// Run the child scenario with `mode` and return the host's stderr.
fn host_log(mode: &str) -> String {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "output_test::worker_prints_a_secret_child",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, mode)
        .env("LOG", "info")
        .env_remove("ENVIRONMENT")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "child scenario failed:\n{stdout}\n{stderr}"
    );
    stderr
}

#[test]
fn worker_output_stays_out_of_the_host_log_by_default() {
    let log = host_log("default");
    assert!(!log.contains(SECRET), "worker output leaked:\n{log}");
}

#[test]
fn enabled_mirroring_copies_worker_output_into_the_host_log() {
    let log = host_log("mirror");
    assert!(
        log.lines()
            .any(|line| line.contains("[worker:") && line.contains(SECRET)),
        "worker output was not mirrored:\n{log}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs in a child process through the tests above"]
async fn worker_prints_a_secret_child() {
    let Some(mode) = std::env::var_os(CHILD) else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    // The worker writes the secret to stderr, then starts normally.
    let uv = uv_script(
        &tmp,
        &format!("echo '{SECRET}' >&2\nexec python3 \"$FAKE_WORKER\" \"$@\""),
    );
    let scripts = scripts_dir(&tmp, &["noisy"]);
    let mut builder = Manager::builder(&scripts).uv_path(&uv);
    if mode == "mirror" {
        builder = builder.mirror_worker_output(true);
    }
    let manager = builder.build().await.unwrap();
    // The handler receives output either way, which also shows the record was
    // dispatched (and mirrored, if enabled) before the host log is inspected.
    let (seen_tx, seen) = mpsc::channel();
    manager.on_worker_output(move |output| {
        if output.line == SECRET {
            let _ = seen_tx.send(());
        }
    });
    let worker = manager.worker("noisy").spawn().await.unwrap();
    seen.recv_timeout(Duration::from_secs(10))
        .expect("the handler received the worker's output");
    worker.terminate().await.unwrap();
}
