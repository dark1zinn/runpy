//! End-to-end tests through real `uv` and the Python SDK from this checkout.
//!
//! Each test stages PEP 723 scripts in a temporary directory whose single
//! dependency is the local `worker/` package (an absolute editable path, no
//! lockfile, no remote source), then drives them through the public Rust API.
//! `uv` must be on `PATH`; the first run builds the package into uv's cache.

use runpy::{
    Cancellation, Data, Envelope, Manager, Meta, PoolConfig, RunpyError, WorkerOutputStream,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(120);

fn sdk_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../worker")
        .canonicalize()
        .expect("worker package exists")
}

/// Stage `name.py` with a PEP 723 header pointing at the local SDK.
fn stage(dir: &Path, name: &str, body: &str) {
    let header = format!(
        "# /// script\n# requires-python = \">=3.10\"\n# dependencies = [\"runpyrs\"]\n#\n\
         # [tool.uv.sources]\n# runpyrs = {{ path = \"{}\", editable = true }}\n# ///\n",
        sdk_path().display()
    );
    std::fs::write(dir.join(format!("{name}.py")), format!("{header}{body}")).unwrap();
}

fn data(value: Value) -> Data {
    value.as_object().cloned().expect("object")
}

const SYNC_WORKER: &str = r#"
import os, sys
import runpyrs
from runpyrs import RunScript, Worker

class Probe(Worker):
    def execute(self, data):
        mode = data.get("mode")
        if mode == "origin":
            return {"file": runpyrs.__file__, "pid": os.getpid()}
        if mode == "log":
            self.log({"step": "logged"}, level="debug")
            return {}
        if mode == "print":
            print("hello from stdout", flush=True)
            print("hello from stderr", file=sys.stderr, flush=True)
            return {}
        if mode == "fail":
            raise ValueError("bad input")
        if mode == "die":
            os._exit(3)
        return {"echo": data}

    def handle_envelope(self, envelope):
        self.send({"reply": envelope["data"]}, meta={"correlation_id": envelope["meta"].get("correlation_id")})

if __name__ == "__main__":
    RunScript(Probe)
"#;

const ASYNC_WORKER: &str = r#"
import asyncio
from runpyrs import AsyncWorker, RunAsyncScript

class Sleeper(AsyncWorker):
    async def execute(self, data):
        await asyncio.sleep(data.get("sleep", 0))
        return {"slept": data.get("sleep", 0)}

if __name__ == "__main__":
    RunAsyncScript(Sleeper)
"#;

async fn manager(dir: &TempDir) -> Manager {
    stage(dir.path(), "probe", SYNC_WORKER);
    stage(dir.path(), "sleeper", ASYNC_WORKER);
    Manager::builder(dir.path())
        .startup_timeout(TIMEOUT)
        .build()
        .await
        .expect("uv must be on PATH")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sdk_comes_from_this_checkout() {
    let dir = TempDir::new().unwrap();
    let manager = manager(&dir).await;
    let worker = manager.worker("probe").spawn().await.unwrap();
    let output = worker
        .execute(data(json!({"mode": "origin"})), TIMEOUT)
        .await
        .unwrap();
    let file = PathBuf::from(output["file"].as_str().unwrap());
    assert!(
        file.starts_with(sdk_path()),
        "runpyrs was imported from {}, not {}",
        file.display(),
        sdk_path().display()
    );
    worker.terminate().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn one_worker_covers_the_full_message_lifecycle() {
    let dir = TempDir::new().unwrap();
    let manager = manager(&dir).await;
    let (messages_tx, messages) = mpsc::channel::<Envelope>();
    let (output_tx, output) = mpsc::channel();
    manager.on_message(move |inbound| {
        let _ = messages_tx.send(Envelope::clone(&inbound.envelope));
    });
    manager.on_worker_output(move |record| {
        let _ = output_tx.send((record.stream, record.line));
    });
    let worker = manager.worker("probe").spawn().await.unwrap();
    assert_eq!(worker.capabilities(), ["requests"]);

    // execute -> done
    let echo = worker
        .execute(data(json!({"n": 1})), TIMEOUT)
        .await
        .unwrap();
    assert_eq!(echo["echo"]["n"], 1);

    // log reaches handlers as a structured envelope
    worker
        .execute(data(json!({"mode": "log"})), TIMEOUT)
        .await
        .unwrap();
    let log = messages
        .iter()
        .find(|envelope| envelope.meta().get("x_op") == Some(&json!("log")))
        .unwrap();
    assert_eq!(log.meta()["level"], "debug");
    assert_eq!(log.data()["step"], "logged");

    // custom envelope round trip with application metadata
    let mut meta = Meta::new();
    meta.insert("correlation_id".into(), json!(42));
    worker
        .send_message(Envelope::new(meta, data(json!({"ping": true}))).unwrap())
        .await
        .unwrap();
    // Handlers also observe request results; the reply is the one without x_op.
    let reply = std::iter::from_fn(|| messages.recv_timeout(Duration::from_secs(10)).ok())
        .find(|envelope| envelope.meta().get("x_op").is_none())
        .expect("custom reply");
    assert_eq!(reply.meta()["correlation_id"], 42);
    assert_eq!(reply.data()["reply"]["ping"], true);
    assert_eq!(reply.meta()["x_wid"], worker.id());

    // process output is captured and attributed
    worker
        .execute(data(json!({"mode": "print"})), TIMEOUT)
        .await
        .unwrap();
    let mut seen = Vec::new();
    while seen.len() < 2 {
        let (stream, line) = output.recv_timeout(Duration::from_secs(10)).unwrap();
        if line.starts_with("hello from") {
            seen.push((stream, line));
        }
    }
    assert!(seen.contains(&(WorkerOutputStream::Stdout, "hello from stdout".to_string())));
    assert!(seen.contains(&(WorkerOutputStream::Stderr, "hello from stderr".to_string())));

    // a Python exception is a structured error and the worker survives
    match worker.execute(data(json!({"mode": "fail"})), TIMEOUT).await {
        Err(RunpyError::Execution { error, .. }) => {
            assert_eq!(
                (error.kind.as_str(), error.message.as_str()),
                ("ValueError", "bad input")
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    worker.execute(data(json!({})), TIMEOUT).await.unwrap();

    // death while running is an unknown outcome and a retained exit
    assert!(matches!(
        worker.execute(data(json!({"mode": "die"})), TIMEOUT).await,
        Err(RunpyError::OutcomeUnknown { .. })
    ));
    let exit = worker.wait().await.unwrap();
    assert_eq!(exit.worker_id, worker.id());

    // shutdown of a healthy worker
    let other = manager.worker("probe").spawn().await.unwrap();
    let exit = other.terminate().await.unwrap();
    assert_eq!(exit.reason, runpy::ExitReason::Terminated);
    assert!(manager.watchdog().report().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn pools_run_requests_on_separate_real_workers() {
    let dir = TempDir::new().unwrap();
    let manager = manager(&dir).await;
    let pool = manager
        .pool(manager.worker("probe"), PoolConfig::default())
        .await
        .unwrap();
    let first = pool
        .submit(data(json!({"mode": "origin"})), TIMEOUT)
        .await
        .unwrap();
    let second = pool
        .submit(data(json!({"mode": "origin"})), TIMEOUT)
        .await
        .unwrap();
    let (first, second) = tokio::join!(first.result(), second.result());
    assert_ne!(first.unwrap()["pid"], second.unwrap()["pid"]);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn async_workers_cancel_running_requests() {
    let dir = TempDir::new().unwrap();
    let manager = manager(&dir).await;
    let worker = manager.worker("sleeper").spawn().await.unwrap();
    assert_eq!(worker.capabilities(), ["requests", "cancel"]);

    let request = worker
        .submit(data(json!({"sleep": 60})), TIMEOUT)
        .await
        .unwrap();
    let canceller = request.canceller();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(canceller.cancel().await.unwrap(), Cancellation::Cancelled);
    assert!(matches!(
        request.result().await,
        Err(RunpyError::Cancelled { .. })
    ));

    let output = worker
        .execute(data(json!({"sleep": 0})), TIMEOUT)
        .await
        .unwrap();
    assert_eq!(output["slept"], 0);
    worker.terminate().await.unwrap();
}
