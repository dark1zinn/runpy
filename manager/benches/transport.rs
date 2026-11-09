//! Transport benchmarks through the public API and the local Python SDK.
//!
//! `uv` is replaced by a shim that runs the worker with the `python3` on
//! `PATH` and this checkout's `worker/src` on `PYTHONPATH`, so the numbers
//! cover framing, sockets, JSON, and the SDK's dispatch, not dependency
//! resolution. `examples/performance` measures real `uv` cold starts.
//!
//! Run with `cargo bench -p runpy --bench transport`.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rand::{Rng, SeedableRng, distributions::Alphanumeric, rngs::StdRng};
use runpy::{Data, Manager, PoolConfig};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use tokio::runtime::Runtime;

const TIMEOUT: Duration = Duration::from_secs(30);
const SIZES: [(&str, usize); 3] = [("1KiB", 1 << 10), ("64KiB", 64 << 10), ("1MiB", 1 << 20)];
const POOL_BATCH: usize = 12;

const ECHO_WORKER: &str = r#"
from runpyrs import RunScript, Worker

class Echo(Worker):
    def execute(self, data):
        return data

if __name__ == "__main__":
    RunScript(Echo)
"#;

/// Deterministic payload of `size` ASCII bytes.
fn payload(size: usize) -> Data {
    let text: String = StdRng::seed_from_u64(0x5EED)
        .sample_iter(&Alphanumeric)
        .take(size)
        .map(char::from)
        .collect();
    json!({ "payload": text }).as_object().cloned().unwrap()
}

struct Fixture {
    _dir: TempDir,
    manager: Manager,
}

fn fixture(runtime: &Runtime) -> Fixture {
    let dir = TempDir::new().unwrap();
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../worker/src");
    let shim = dir.path().join("uv");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\n[ \"$1\" = \"--version\" ] && exit 0\n\
             while [ \"$1\" != \"--script\" ]; do shift; done\nshift\n\
             PYTHONPATH='{}' exec python3 \"$@\"\n",
            sdk.canonicalize().unwrap().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.path().join("echo.py"), ECHO_WORKER).unwrap();
    let manager = runtime.block_on(async {
        Manager::builder(dir.path())
            .uv_path(&shim)
            .build()
            .await
            .unwrap()
    });
    Fixture { _dir: dir, manager }
}

fn request_round_trip(c: &mut Criterion) {
    let runtime = Runtime::new().unwrap();
    let fixture = fixture(&runtime);
    let worker = runtime
        .block_on(fixture.manager.worker("echo").spawn())
        .unwrap();
    let mut group = c.benchmark_group("request_round_trip");
    for (name, size) in SIZES {
        let data = payload(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), &data, |b, data| {
            b.to_async(&runtime)
                .iter(|| async { worker.execute(data.clone(), TIMEOUT).await.unwrap() });
        });
    }
    group.finish();
    runtime.block_on(worker.terminate()).unwrap();
}

fn cold_worker(c: &mut Criterion) {
    let runtime = Runtime::new().unwrap();
    let fixture = fixture(&runtime);
    let data = payload(1 << 10);
    c.bench_function("cold_worker/spawn_request_terminate", |b| {
        b.to_async(&runtime).iter(|| async {
            let worker = fixture.manager.worker("echo").spawn().await.unwrap();
            worker.execute(data.clone(), TIMEOUT).await.unwrap();
            worker.terminate().await.unwrap();
        });
    });
}

fn pool_throughput(c: &mut Criterion) {
    let runtime = Runtime::new().unwrap();
    let fixture = fixture(&runtime);
    let data = payload(1 << 10);
    let mut group = c.benchmark_group("pool_throughput");
    group.throughput(Throughput::Elements(POOL_BATCH as u64));
    for width in 1..=3 {
        let config = PoolConfig::default()
            .min_workers(width)
            .max_workers(width)
            .reuse_workers(true);
        let pool = runtime
            .block_on(fixture.manager.pool(fixture.manager.worker("echo"), config))
            .unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(width), &width, |b, _| {
            b.to_async(&runtime).iter(|| async {
                let mut requests = Vec::with_capacity(POOL_BATCH);
                for _ in 0..POOL_BATCH {
                    requests.push(pool.submit(data.clone(), TIMEOUT).await.unwrap());
                }
                for request in requests {
                    request.result().await.unwrap();
                }
            });
        });
        runtime.block_on(pool.close());
    }
    group.finish();
}

fn quiet() -> Criterion {
    if std::env::var_os("LOG").is_none() {
        // SAFETY: runs before any runtime or other thread exists.
        unsafe { std::env::set_var("LOG", "off") };
    }
    Criterion::default()
        .sample_size(20)
        .measurement_time(Duration::from_secs(5))
}

criterion_group! {
    name = transport;
    config = quiet();
    targets = request_round_trip, cold_worker, pool_throughput
}
criterion_main!(transport);
