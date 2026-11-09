use runpy::{Data, Manager, PoolConfig, RunpyError, TimeoutStage, WorkerPool};
use serde_json::{Value, json};
use std::fs;
use std::time::{Duration, Instant};
use tempfile::TempDir;

use crate::manager_test::{counting_uv, fake_uv, launch_count, scripts_dir};

const LONG: Duration = Duration::from_secs(10);

fn data(value: Value) -> Data {
    value
        .as_object()
        .cloned()
        .expect("test data must be an object")
}

struct Fixture {
    tmp: TempDir,
    manager: Manager,
}

impl Fixture {
    async fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let launches = tmp.path().join("launches");
        let uv = counting_uv(&tmp, &launches);
        let scripts = scripts_dir(&tmp, &["parse"]);
        let manager = Manager::builder(&scripts)
            .uv_path(&uv)
            .build()
            .await
            .unwrap();
        Self { tmp, manager }
    }

    async fn pool(&self, config: PoolConfig) -> WorkerPool {
        self.manager
            .pool(self.manager.worker("parse"), config)
            .await
            .unwrap()
    }

    fn launches(&self) -> usize {
        launch_count(&self.tmp.path().join("launches"))
    }

    fn path(&self, name: &str) -> String {
        self.tmp.path().join(name).to_string_lossy().into_owned()
    }

    /// `(event, pid, time)` lines written by the fake worker's `record`.
    fn events(&self, name: &str) -> Vec<(String, u32, f64)> {
        fs::read_to_string(self.tmp.path().join(name))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let mut parts = line.split(' ');
                (
                    parts.next().unwrap().to_string(),
                    parts.next().unwrap().parse().unwrap(),
                    parts.next().unwrap().parse().unwrap(),
                )
            })
            .collect()
    }

    async fn wait_until_idle(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.manager.watchdog().report().await.is_empty() {
            assert!(Instant::now() < deadline, "pool workers were not retired");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn independent_requests_run_concurrently_on_fresh_workers() {
    let fixture = Fixture::new().await;
    let pool = fixture.pool(PoolConfig::default()).await;
    // Creating a pool starts nothing.
    assert_eq!(fixture.launches(), 0);
    assert!(fixture.manager.watchdog().report().await.is_empty());

    let barrier = fixture.tmp.path().join("barrier");
    fs::create_dir(&barrier).unwrap();
    let request = |text: &str, sleep: f64| {
        data(json!({
            "text": text,
            "barrier": barrier.to_string_lossy(),
            "parties": 2,
            "sleep": sleep,
        }))
    };
    // Each request waits at the barrier for the other, so both must be
    // executing at once; the first finishes last.
    let first = pool.submit(request("first", 0.3), LONG).await.unwrap();
    let second = pool.submit(request("second", 0.0), LONG).await.unwrap();
    assert_ne!(first.id(), second.id());
    let (first, second) = tokio::join!(first.result(), second.result());
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(first["echo"]["text"], "first");
    assert_eq!(second["echo"]["text"], "second");
    assert_ne!(first["pid"], second["pid"]);

    // Cold pools never reuse a process.
    fixture.wait_until_idle().await;
    let third = pool.execute(data(json!({})), LONG).await.unwrap();
    assert_ne!(third["pid"], first["pid"]);
    assert_ne!(third["pid"], second["pid"]);
    assert_eq!(fixture.launches(), 3);
    fixture.wait_until_idle().await;
}

#[cfg(unix)]
#[tokio::test]
async fn pool_max_workers_one_serializes_requests_in_fifo_order() {
    let fixture = Fixture::new().await;
    let pool = fixture.pool(PoolConfig::default().max_workers(1)).await;
    let log = fixture.path("events");
    let mut requests = Vec::new();
    for index in 0..3 {
        let input = data(json!({ "index": index, "record": log, "sleep": 0.1 }));
        requests.push(pool.submit(input, LONG).await.unwrap());
    }
    for (index, request) in requests.into_iter().enumerate() {
        assert_eq!(request.result().await.unwrap()["echo"]["index"], index);
    }

    let events = fixture.events("events");
    let order: Vec<&str> = events.iter().map(|(event, ..)| event.as_str()).collect();
    assert_eq!(order, ["start", "end", "start", "end", "start", "end"]);
}

#[cfg(unix)]
#[tokio::test]
async fn queued_expiry_never_starts_a_worker() {
    let fixture = Fixture::new().await;
    let pool = fixture.pool(PoolConfig::default().max_workers(1)).await;
    let busy = pool
        .submit(data(json!({ "sleep": 0.6 })), LONG)
        .await
        .unwrap();
    let queued = pool
        .submit(data(json!({})), Duration::from_millis(200))
        .await
        .unwrap();

    assert!(matches!(
        queued.result().await,
        Err(RunpyError::Timeout(TimeoutStage::Queued))
    ));
    busy.result().await.unwrap();
    fixture.wait_until_idle().await;
    assert_eq!(fixture.launches(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn running_timeout_keeps_the_pool_slot_busy() {
    let fixture = Fixture::new().await;
    let pool = fixture.pool(PoolConfig::default().max_workers(1)).await;
    let log = fixture.path("events");
    let slow = pool
        .submit(
            data(json!({ "record": log, "sleep": 0.6 })),
            Duration::from_millis(300),
        )
        .await
        .unwrap();
    assert!(matches!(
        slow.result().await,
        Err(RunpyError::Timeout(TimeoutStage::Response))
    ));
    pool.execute(data(json!({ "record": log })), LONG)
        .await
        .unwrap();

    let events = fixture.events("events");
    let slow_end = events
        .iter()
        .find(|(event, ..)| event == "end")
        .map(|(.., time)| *time)
        .unwrap();
    let next_start = events
        .iter()
        .filter(|(event, ..)| event == "start")
        .nth(1)
        .map(|(.., time)| *time)
        .unwrap();
    assert!(
        next_start >= slow_end,
        "a timed-out request's worker was reused"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn full_queue_rejects_or_times_out_admission() {
    let fixture = Fixture::new().await;
    let pool = fixture
        .pool(PoolConfig::default().max_workers(1).queue_capacity(1))
        .await;
    let running = pool
        .submit(data(json!({ "sleep": 0.5 })), LONG)
        .await
        .unwrap();
    // Wait until the first request left the queue for the only worker.
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture.launches() == 0 {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let queued = pool.submit(data(json!({})), LONG).await.unwrap();

    assert!(matches!(
        pool.try_submit(data(json!({})), LONG),
        Err(RunpyError::QueueFull)
    ));
    assert!(matches!(
        pool.submit(data(json!({})), Duration::from_millis(100))
            .await,
        Err(RunpyError::Timeout(TimeoutStage::Admission))
    ));
    running.result().await.unwrap();
    queued.result().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn close_fails_queued_work_and_stops_running_workers() {
    let fixture = Fixture::new().await;
    let pool = fixture.pool(PoolConfig::default().max_workers(1)).await;
    let running = pool
        .submit(data(json!({ "sleep": 30 })), LONG)
        .await
        .unwrap();
    let queued = pool.submit(data(json!({})), LONG).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture.manager.watchdog().report().await.is_empty() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Let the running request reach the worker.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let exits = pool.close().await;
    assert_eq!(exits.len(), 1);
    assert!(matches!(queued.result().await, Err(RunpyError::PoolClosed)));
    assert!(matches!(
        running.result().await,
        Err(RunpyError::OutcomeUnknown { .. })
    ));
    assert!(matches!(
        pool.submit(data(json!({})), LONG).await,
        Err(RunpyError::PoolClosed)
    ));
    assert!(fixture.manager.watchdog().report().await.is_empty());
    assert_eq!(fixture.launches(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn pools_share_the_manager_worker_cap() {
    let fixture = Fixture::new().await;
    let first = fixture.pool(PoolConfig::default()).await;
    let second = fixture.pool(PoolConfig::default()).await;
    assert_ne!(first.id(), second.id());
    let barrier = fixture.tmp.path().join("barrier");
    fs::create_dir(&barrier).unwrap();
    let request =
        || data(json!({ "barrier": barrier.to_string_lossy(), "parties": 3, "sleep": 0.3 }));

    let mut requests = Vec::new();
    for pool in [&first, &second, &first, &second] {
        requests.push(pool.submit(request(), LONG).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Three requests meet at the barrier; the fourth waits for a slot.
    assert_eq!(fixture.launches(), 3);
    assert_eq!(fixture.manager.watchdog().report().await.len(), 3);
    for request in requests {
        request.result().await.unwrap();
    }
    assert_eq!(fixture.launches(), 4);
}

#[tokio::test]
async fn invalid_pool_configuration_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let uv = fake_uv(&tmp);
    let scripts = scripts_dir(&tmp, &["parse"]);
    let manager = Manager::builder(&scripts)
        .uv_path(&uv)
        .build()
        .await
        .unwrap();
    let other = Manager::builder(&scripts)
        .uv_path(&uv)
        .build()
        .await
        .unwrap();

    for config in [
        PoolConfig::default().max_workers(0),
        PoolConfig::default().max_workers(4),
        PoolConfig::default().queue_capacity(0),
    ] {
        assert!(matches!(
            manager.pool(manager.worker("parse"), config).await,
            Err(RunpyError::InvalidConfig(_))
        ));
    }
    assert!(matches!(
        manager
            .pool(other.worker("parse"), PoolConfig::default())
            .await,
        Err(RunpyError::InvalidConfig(_))
    ));
}
