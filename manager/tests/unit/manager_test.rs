/// Unit tests for the Manager (lib.rs) and Worker (manager.rs).
///
/// These tests exercise the public API: Manager creation, Worker builder
/// pattern, message sending before/after spawn, env vars, handlers, and
/// the Manager Drop behaviour.
use runpy::{Manager, Message, Worker, WorkerIdentity};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

// ─── WorkerIdentity ────────────────────────────────────────────────────

#[test]
fn worker_identity_contains_script_name() {
    let id = WorkerIdentity::new("my_script");
    assert!(
        id.name.starts_with("my_script_"),
        "Identity name should start with the script name, got: {}",
        id.name
    );
}

#[test]
fn worker_identity_sock_file_has_prefix_and_suffix() {
    let id = WorkerIdentity::new("test");
    assert!(id.sock_file.starts_with("rp_"));
    assert!(id.sock_file.ends_with(".sock"));
}

#[test]
fn worker_identity_is_unique() {
    let id1 = WorkerIdentity::new("same");
    let id2 = WorkerIdentity::new("same");
    // The random suffix makes them different (overwhelmingly likely)
    assert_ne!(id1.name, id2.name);
    assert_ne!(id1.sock_file, id2.sock_file);
}

#[test]
fn worker_identity_clone() {
    let id = WorkerIdentity::new("cloneable");
    let cloned = id.clone();
    assert_eq!(id.name, cloned.name);
    assert_eq!(id.sock_file, cloned.sock_file);
}

// ─── Manager creation ──────────────────────────────────────────────────

#[tokio::test]
async fn manager_new_with_invalid_venv_does_not_panic() {
    // Manager::new logs an error but does not panic
    let _manager = Manager::new("/nonexistent/venv", "/nonexistent/scripts");
    // If we get here, it didn't panic — that's the test.
}

#[tokio::test]
async fn manager_new_with_valid_paths() {
    let tmp = tempfile::TempDir::new().unwrap();

    // Create a fake venv
    let venv = tmp.path().join("venv");
    let bin = venv.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/bin/sh", bin.join("python")).unwrap();

    // Create a scripts dir
    let scripts = tmp.path().join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::write(scripts.join("hello.py"), "# stub").unwrap();

    let manager = Manager::new(venv.to_str().unwrap(), scripts.to_str().unwrap());

    // check_integrity should succeed
    assert!(manager.check_integrity().is_ok());
}

// ─── Manager::check_integrity ──────────────────────────────────────────

#[tokio::test]
async fn check_integrity_fails_on_invalid_venv() {
    let manager = Manager::new("/nonexistent/venv", "/nonexistent/scripts");
    let result = manager.check_integrity();
    assert!(result.is_err());
}

// ─── Manager::on_message ───────────────────────────────────────────────

#[tokio::test]
async fn manager_on_message_registers_handler() {
    let tmp = tempfile::TempDir::new().unwrap();
    let venv = tmp.path().join("venv");
    std::fs::create_dir_all(venv.join("bin")).unwrap();
    std::os::unix::fs::symlink("/bin/sh", venv.join("bin/python")).unwrap();
    let scripts = tmp.path().join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();

    let mut manager = Manager::new(venv.to_str().unwrap(), scripts.to_str().unwrap());

    let called = Arc::new(AtomicBool::new(false));
    let called_clone = called.clone();

    manager.on_message(move |_envelope| {
        called_clone.store(true, Ordering::SeqCst);
    });

    // We can't trigger the handler without a real worker connection,
    // but we can verify the manager accepts the handler without error.
    // The handler is tested more thoroughly in the integration test.
}

// ─── Manager::worker (builder) ─────────────────────────────────────────

#[tokio::test]
async fn manager_worker_returns_worker_builder() {
    let manager = Manager::new("/fake/venv", "/fake/scripts");
    let _worker: Worker = manager.worker("some_script");
    // If this compiles and runs, the builder was created.
}

// ─── Worker builder methods ────────────────────────────────────────────

#[tokio::test]
async fn worker_env_is_chainable() {
    let manager = Manager::new("/fake/venv", "/fake/scripts");
    let mut worker = manager.worker("test");
    // env() returns &mut Self, so chaining should work
    worker.env("A", "1").env("B", "2").env("C", "3");
    // No panic = success
}

#[tokio::test]
async fn worker_on_message_is_chainable() {
    let manager = Manager::new("/fake/venv", "/fake/scripts");
    let mut worker = manager.worker("test");
    worker.on_message(|_env| {
        println!("handler 1");
    });
    // Setting a new handler replaces the old one (no panic)
    worker.on_message(|_env| {
        println!("handler 2");
    });
}

// ─── Worker::send_message before spawn ─────────────────────────────────

#[tokio::test]
async fn send_message_before_spawn_returns_error() {
    let manager = Manager::new("/fake/venv", "/fake/scripts");
    let worker = manager.worker("test");

    let result = worker
        .send_message(&Message::Terminate)
        .await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not been spawned"));
}

// ─── Worker::terminate before spawn ────────────────────────────────────

#[tokio::test]
async fn terminate_before_spawn_returns_error() {
    let manager = Manager::new("/fake/venv", "/fake/scripts");
    let worker = manager.worker("test");

    let result = worker.terminate().await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not been spawned"));
}

// ─── Worker::spawn with nonexistent python ─────────────────────────────

#[tokio::test]
async fn spawn_with_nonexistent_python_returns_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    let scripts = tmp.path().join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::write(scripts.join("test.py"), "# stub").unwrap();

    // Socket dir must exist
    std::fs::create_dir_all("/tmp/runpy").ok();

    let manager = Manager::new("/nonexistent/venv", scripts.to_str().unwrap());
    let mut worker = manager.worker("test");
    let result = worker.spawn().await;
    // Should fail — either the socket bind fails (if /tmp/runpy was cleaned)
    // or the python binary is not found. Either way it must be an error.
    assert!(result.is_err(), "Expected error, got Ok({:?})", result.ok());
}

// ─── Manager Drop does not panic ───────────────────────────────────────

#[tokio::test]
async fn manager_drop_does_not_panic_without_workers() {
    let manager = Manager::new("/fake/venv", "/fake/scripts");
    drop(manager);
    // No panic = success
}

#[tokio::test]
async fn manager_drop_does_not_panic_in_async_context() {
    {
        let _manager = Manager::new("/fake/venv", "/fake/scripts");
        // Manager is dropped at end of this block, inside an async runtime.
    }
    // If we reach here, try_write() in Drop worked without panicking.
}

// ─── Watchdog report on empty manager ──────────────────────────────────

#[tokio::test]
async fn watchdog_report_empty_when_no_workers() {
    let manager = Manager::new("/fake/venv", "/fake/scripts");
    let reports = manager.dog.report().await;
    assert!(reports.is_empty());
}
