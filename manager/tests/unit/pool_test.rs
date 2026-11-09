use runpy::{Data, Manager, ManagerBuilder, PoolConfig, RunpyError, TimeoutStage, WorkerPool};
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
        Self::with(|builder| builder).await
    }

    async fn with(configure: impl FnOnce(ManagerBuilder) -> ManagerBuilder) -> Self {
        let tmp = TempDir::new().unwrap();
        let launches = tmp.path().join("launches");
        let uv = counting_uv(&tmp, &launches);
        let scripts = scripts_dir(&tmp, &["parse"]);
        let manager = configure(Manager::builder(&scripts).uv_path(&uv))
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

/// Wait until `check` holds, polling briefly.
async fn eventually(mut check: impl AsyncFnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check().await {
        assert!(Instant::now() < deadline, "{what} did not happen");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn warm(min: usize) -> PoolConfig {
    PoolConfig::default().min_workers(min).reuse_workers(true)
}

#[cfg(unix)]
#[tokio::test]
async fn warm_minimum_starts_eagerly_and_its_slots_are_not_borrowed() {
    let fixture =
        Fixture::with(|builder| builder.startup_timeout(Duration::from_millis(500))).await;
    let pool = fixture.pool(warm(2)).await;
    // Creation waited for both minimum workers.
    assert_eq!(fixture.launches(), 2);
    assert_eq!(fixture.manager.watchdog().report().await.len(), 2);

    // Only the one unreserved slot is available to direct spawns.
    let direct = fixture.manager.worker("parse").spawn().await.unwrap();
    assert!(matches!(
        fixture.manager.worker("parse").spawn().await,
        Err(RunpyError::Timeout(TimeoutStage::Admission))
    ));

    // Requests run on the warm workers without starting more.
    let output = pool.execute(data(json!({})), LONG).await.unwrap();
    let pids: Vec<u32> = fixture
        .manager
        .watchdog()
        .report()
        .await
        .iter()
        .map(|report| report.pid)
        .collect();
    assert_eq!(fixture.launches(), 3);
    assert!(output["pid"].as_u64().is_some());
    assert_eq!(pids.len(), 3);

    // Closing returns the reservation: three direct spawns fit again.
    assert_eq!(pool.close().await.len(), 2);
    direct.terminate().await.unwrap();
    for _ in 0..3 {
        fixture.manager.worker("parse").spawn().await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn failed_warm_start_rolls_back_the_reservation() {
    let fixture =
        Fixture::with(|builder| builder.startup_timeout(Duration::from_millis(500))).await;
    let template = fixture
        .manager
        .worker("parse")
        .env("RUNPY_TEST_MODE", "exit");
    assert!(matches!(
        fixture.manager.pool(template, warm(2)).await,
        Err(RunpyError::WorkerExited { .. })
    ));
    assert!(fixture.manager.watchdog().report().await.is_empty());
    // Neither the reserved slots nor the minima accounting leaked.
    let _pool = fixture.pool(warm(3)).await;
    assert_eq!(fixture.manager.watchdog().report().await.len(), 3);
}

#[cfg(unix)]
#[tokio::test]
async fn lost_minimum_worker_is_replaced_once_or_the_pool_closes() {
    let fixture = Fixture::new().await;
    let pool = fixture.pool(warm(1).max_workers(1)).await;
    let first = pool.execute(data(json!({})), LONG).await.unwrap()["pid"]
        .as_u64()
        .unwrap();

    // The worker dies while idle and is replaced by a new process.
    unsafe { libc::kill(first as libc::pid_t, libc::SIGKILL) };
    eventually(async || fixture.launches() == 2, "replacement").await;
    let second = pool.execute(data(json!({})), LONG).await.unwrap()["pid"]
        .as_u64()
        .unwrap();
    assert_ne!(first, second);

    // If the single replacement attempt fails, the pool closes instead of
    // retrying in a loop.
    fs::write(fixture.tmp.path().join("launches.fail"), "").unwrap();
    unsafe { libc::kill(second as libc::pid_t, libc::SIGKILL) };
    eventually(
        async || {
            matches!(
                pool.submit(data(json!({})), LONG).await,
                Err(RunpyError::PoolClosed)
            )
        },
        "pool closing",
    )
    .await;
    assert_eq!(fixture.launches(), 3);
    pool.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn waiting_pool_gets_the_next_slot_before_a_hot_pool() {
    let fixture = Fixture::new().await;
    let hot = fixture.pool(PoolConfig::default()).await;
    let other = fixture.pool(PoolConfig::default()).await;
    let log = fixture.path("events");

    let mut hot_requests = Vec::new();
    for index in 0..6 {
        let input = data(json!({ "pool": "hot", "index": index, "record": log, "sleep": 0.4 }));
        hot_requests.push(hot.submit(input, LONG).await.unwrap());
    }
    eventually(async || fixture.launches() == 3, "hot pool saturation").await;
    let other_request = other
        .submit(data(json!({ "pool": "other", "record": log })), LONG)
        .await
        .unwrap();
    let other_pid = other_request.result().await.unwrap()["pid"]
        .as_u64()
        .unwrap() as u32;
    for request in hot_requests {
        request.result().await.unwrap();
    }

    // The other pool started before the hot pool's fourth request.
    let starts: Vec<u32> = fixture
        .events("events")
        .into_iter()
        .filter(|(event, ..)| event == "start")
        .map(|(_, pid, _)| pid)
        .collect();
    let position = starts.iter().position(|pid| *pid == other_pid).unwrap();
    assert_eq!(position, 3, "start order: {starts:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn idle_surplus_is_retired_to_serve_another_pool() {
    let fixture = Fixture::new().await;
    let reusing = fixture
        .pool(
            PoolConfig::default()
                .reuse_workers(true)
                .idle_timeout(LONG * 6),
        )
        .await;
    let barrier = fixture.tmp.path().join("barrier");
    fs::create_dir(&barrier).unwrap();
    let mut requests = Vec::new();
    for _ in 0..3 {
        let input = data(json!({ "barrier": barrier.to_string_lossy(), "parties": 3 }));
        requests.push(reusing.submit(input, LONG).await.unwrap());
    }
    for request in requests {
        request.result().await.unwrap();
    }
    // Three idle workers now hold every Manager-wide slot.
    assert_eq!(fixture.manager.watchdog().report().await.len(), 3);

    let other = fixture.pool(PoolConfig::default()).await;
    other
        .execute(data(json!({})), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(fixture.launches(), 4);
}

#[tokio::test]
async fn invalid_minima_are_rejected_before_starting_anything() {
    let fixture = Fixture::new().await;
    for config in [PoolConfig::default().min_workers(1), warm(3).max_workers(2)] {
        assert!(matches!(
            fixture
                .manager
                .pool(fixture.manager.worker("parse"), config)
                .await,
            Err(RunpyError::InvalidConfig(_))
        ));
    }
    let _first = fixture.pool(warm(2)).await;
    assert!(matches!(
        fixture
            .manager
            .pool(fixture.manager.worker("parse"), warm(2))
            .await,
        Err(RunpyError::InvalidConfig(_))
    ));
    assert_eq!(fixture.launches(), 2);
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

#[cfg(unix)]
#[tokio::test]
async fn reuse_keeps_a_finished_worker_for_the_next_request() {
    let fixture = Fixture::new().await;
    let pool = fixture
        .pool(PoolConfig::default().max_workers(1).reuse_workers(true))
        .await;
    let first = pool.execute(data(json!({})), LONG).await.unwrap();
    let second = pool.execute(data(json!({ "n": 2 })), LONG).await.unwrap();
    assert_eq!(first["pid"], second["pid"]);
    assert_eq!(fixture.launches(), 1);
    assert_eq!(fixture.manager.watchdog().report().await.len(), 1);

    assert_eq!(pool.close().await.len(), 1);
    assert!(fixture.manager.watchdog().report().await.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn idle_workers_retire_after_the_timeout_but_not_while_busy() {
    let fixture = Fixture::new().await;
    let pool = fixture
        .pool(
            PoolConfig::default()
                .reuse_workers(true)
                .idle_timeout(Duration::from_millis(300)),
        )
        .await;
    let first = pool.execute(data(json!({})), LONG).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    // This request outlives the first idle timer; that stale timer must not
    // stop the busy worker.
    let second = pool
        .execute(data(json!({ "sleep": 0.4 })), LONG)
        .await
        .unwrap();
    assert_eq!(first["pid"], second["pid"]);
    assert_eq!(fixture.manager.watchdog().report().await.len(), 1);

    // Idle again, it is retired once a full timeout passes.
    fixture.wait_until_idle().await;
    assert_eq!(fixture.launches(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn failed_workers_are_never_reused() {
    let fixture = Fixture::new().await;
    let pool = fixture
        .pool(PoolConfig::default().max_workers(1).reuse_workers(true))
        .await;
    let first = pool.execute(data(json!({})), LONG).await.unwrap();
    assert!(matches!(
        pool.execute(data(json!({ "exit": true })), LONG).await,
        Err(RunpyError::OutcomeUnknown { .. })
    ));
    let third = pool.execute(data(json!({})), LONG).await.unwrap();
    assert_ne!(first["pid"], third["pid"]);
    assert_eq!(fixture.launches(), 2);
    pool.close().await;
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
        PoolConfig::default().idle_timeout(Duration::ZERO),
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
