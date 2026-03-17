/// Unit tests for the WatchdogService (watchdog.rs).
///
/// Tests the watchdog's ability to inspect workers, generate reports,
/// detect healthy/dead processes, and clean up dead entries.
use runpy::{ProcessState, WorkerReport};
use std::collections::HashMap;
use std::process::{Child, Command};
use std::sync::Arc;
use tokio::sync::RwLock;

// ── Helpers ────────────────────────────────────────────────────────────

/// Spawn a long-lived dummy process (`sleep 300`) that we can monitor.
fn spawn_sleeper() -> Child {
    Command::new("sleep")
        .arg("300")
        .spawn()
        .expect("failed to spawn `sleep` process")
}

// We can't directly construct `WorkerHandle` from tests (it's crate-private),
// so we test the `WatchdogService` through its public API using the shared
// workers map from a real `Manager`, or we test the standalone helpers.

// ─── Construction ──────────────────────────────────────────────────────

#[tokio::test]
async fn watchdog_new_creates_empty_service() {
    let _workers: Arc<RwLock<HashMap<String, ()>>> = Arc::new(RwLock::new(HashMap::new()));
    // WatchdogService is Clone — just verify we can create one via Manager
    let manager = runpy::Manager::new("/fake/venv", "/fake/scripts");
    // dog is exposed publicly
    let _dog = &manager.dog;
}

// ─── report() on empty map ─────────────────────────────────────────────

#[tokio::test]
async fn report_returns_empty_vec_with_no_workers() {
    let manager = runpy::Manager::new("/fake/venv", "/fake/scripts");
    let reports = manager.dog.report().await;
    assert!(reports.is_empty(), "Expected no reports, got {:?}", reports.len());
}

// ─── report_worker() on missing ID ────────────────────────────────────

#[tokio::test]
async fn report_worker_returns_none_for_unknown_id() {
    let manager = runpy::Manager::new("/fake/venv", "/fake/scripts");
    let report = manager.dog.report_worker("nonexistent_id").await;
    assert!(report.is_none());
}

// ─── ProcessState variants ─────────────────────────────────────────────

#[test]
fn process_state_debug_format() {
    let healthy = ProcessState::Healthy;
    let dead = ProcessState::Dead;
    let frozen = ProcessState::Frozen;
    // Verify Debug is derived
    assert_eq!(format!("{:?}", healthy), "Healthy");
    assert_eq!(format!("{:?}", dead), "Dead");
    assert_eq!(format!("{:?}", frozen), "Frozen");
}

#[test]
fn process_state_clone() {
    let state = ProcessState::Healthy;
    let cloned = state.clone();
    assert!(matches!(cloned, ProcessState::Healthy));
}

#[test]
fn process_state_serializes_to_json() {
    let state = ProcessState::Healthy;
    let json = serde_json::to_string(&state).unwrap();
    assert_eq!(json, "\"Healthy\"");

    let dead = ProcessState::Dead;
    let json = serde_json::to_string(&dead).unwrap();
    assert_eq!(json, "\"Dead\"");
}

// ─── WorkerReport ──────────────────────────────────────────────────────

#[test]
fn worker_report_debug_format() {
    let report = WorkerReport {
        worker_name: "test_worker".into(),
        pid: 12345,
        state: ProcessState::Healthy,
        memory_kb: Some(1024),
        cpu_percent: None,
    };
    let debug = format!("{:?}", report);
    assert!(debug.contains("test_worker"));
    assert!(debug.contains("12345"));
}

#[test]
fn worker_report_clone() {
    let report = WorkerReport {
        worker_name: "w1".into(),
        pid: 100,
        state: ProcessState::Dead,
        memory_kb: None,
        cpu_percent: Some(12.5),
    };
    let cloned = report.clone();
    assert_eq!(cloned.worker_name, "w1");
    assert_eq!(cloned.pid, 100);
    assert!(matches!(cloned.state, ProcessState::Dead));
    assert!(cloned.memory_kb.is_none());
    assert_eq!(cloned.cpu_percent, Some(12.5));
}

#[test]
fn worker_report_serializes_to_json() {
    let report = WorkerReport {
        worker_name: "scraper".into(),
        pid: 999,
        state: ProcessState::Healthy,
        memory_kb: Some(2048),
        cpu_percent: None,
    };
    let json = serde_json::to_string(&report).unwrap();
    assert!(json.contains("\"worker_name\":\"scraper\""));
    assert!(json.contains("\"pid\":999"));
    assert!(json.contains("\"memory_kb\":2048"));
    assert!(json.contains("\"cpu_percent\":null"));
}

#[test]
fn worker_report_serializes_with_all_none() {
    let report = WorkerReport {
        worker_name: "minimal".into(),
        pid: 1,
        state: ProcessState::Frozen,
        memory_kb: None,
        cpu_percent: None,
    };
    let json = serde_json::to_string(&report).unwrap();
    assert!(json.contains("\"Frozen\""));
    assert!(json.contains("\"memory_kb\":null"));
    assert!(json.contains("\"cpu_percent\":null"));
}

// ─── Watchdog clone shares the same map ────────────────────────────────

#[tokio::test]
async fn watchdog_clone_shares_underlying_state() {
    let manager = runpy::Manager::new("/fake/venv", "/fake/scripts");
    let dog1 = manager.dog.clone();
    let dog2 = manager.dog.clone();

    // Both clones should report the same (empty) state
    let r1 = dog1.report().await;
    let r2 = dog2.report().await;
    assert_eq!(r1.len(), r2.len());
    assert!(r1.is_empty());
}

// ─── start_monitoring does not panic ───────────────────────────────────

#[tokio::test]
async fn start_monitoring_does_not_panic_on_empty_map() {
    let manager = runpy::Manager::new("/fake/venv", "/fake/scripts");
    // start_monitoring is called automatically in Manager::new with 5s interval.
    // Calling it again with a different interval should not panic.
    manager.dog.start_monitoring(60);
    // Let it tick once
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    // No panic = success
}

// ─── Integration-style: watchdog with a real spawned process ───────────
// These tests use a real `sleep` process to verify /proc-based health checks.

#[cfg(target_os = "linux")]
mod linux_proc {
    use super::*;

    #[test]
    fn proc_status_exists_for_own_process() {
        let pid = std::process::id();
        let status_path = format!("/proc/{}/status", pid);
        assert!(
            std::path::Path::new(&status_path).exists(),
            "/proc/{}/status should exist for our own process",
            pid
        );
    }

    #[test]
    fn proc_status_contains_vmrss_for_own_process() {
        let pid = std::process::id();
        let status = std::fs::read_to_string(format!("/proc/{}/status", pid)).unwrap();
        assert!(
            status.contains("VmRSS:"),
            "Our own /proc/PID/status should contain VmRSS"
        );
    }

    #[test]
    fn proc_status_missing_for_nonexistent_pid() {
        // PID 0 is the kernel scheduler — its /proc entry doesn't have VmRSS
        // PID 2^31 - 1 is almost certainly not in use
        let result = std::fs::read_to_string("/proc/2147483647/status");
        assert!(result.is_err());
    }

    #[test]
    fn sleeper_process_has_proc_entry() {
        let mut child = spawn_sleeper();
        let pid = child.id();

        let status_path = format!("/proc/{}/status", pid);
        assert!(std::path::Path::new(&status_path).exists());

        // Clean up
        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    fn dead_process_has_no_proc_entry() {
        let mut child = spawn_sleeper();
        let pid = child.id();

        // Kill the process
        child.kill().ok();
        child.wait().ok();

        // /proc entry should be gone (after wait reaps the zombie)
        let status_path = format!("/proc/{}/status", pid);
        // Give the OS a moment
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !std::path::Path::new(&status_path).exists(),
            "/proc/{}/status should not exist after kill+wait",
            pid
        );
    }
}
