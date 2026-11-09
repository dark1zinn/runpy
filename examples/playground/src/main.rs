use runpy::{Data, Envelope, Manager, Meta};
use serde_json::{json, Value};

fn object(value: Value) -> Data {
    value
        .as_object()
        .cloned()
        .expect("example payloads must be JSON objects")
}

#[tokio::main]
async fn main() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let scripts_path = format!("{}/worker", manifest_dir);

    let manager = match Manager::builder(&scripts_path).build().await {
        Ok(manager) => manager,
        Err(error) => {
            eprintln!("Failed to start Runpy: {error}");
            return;
        }
    };
    let logger = manager.logger();
    logger.success("Manager initialized");

    let global_logger = logger.clone();
    manager.on_message(move |inbound| {
        let worker_id = inbound
            .envelope
            .meta()
            .get("x_wid")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        global_logger.verbose_with(
            "Global",
            &format!("Worker '{worker_id}' -> {:?}", inbound.envelope),
        );
    });

    let worker_logger = logger.clone();
    let builder = manager
        .worker("my_script")
        .env("MY_ENV_VAR", "some_value")
        .on_message(move |inbound| {
            let operation = inbound.envelope.meta().get("x_op").and_then(Value::as_str);

            match operation {
                Some("log") => {
                    let level = inbound
                        .envelope
                        .meta()
                        .get("level")
                        .and_then(Value::as_str)
                        .unwrap_or("info");
                    worker_logger.info_with(level, &format!("{:?}", inbound.envelope.data()));
                }
                Some("done") => {
                    worker_logger.success(&format!("Done: {:?}", inbound.envelope.data()));
                }
                Some("error") => {
                    worker_logger.error_with("Worker", &format!("{:?}", inbound.envelope.data()));
                }
                None => {
                    worker_logger.info_with(
                        "Custom",
                        &format!(
                            "meta={:?} data={:?}",
                            inbound.envelope.meta(),
                            inbound.envelope.data()
                        ),
                    );
                }
                Some(other) => {
                    worker_logger.warning_with("Worker", &format!("Unexpected operation: {other}"));
                }
            }
        });

    let worker = match builder.spawn().await {
        Ok(worker) => {
            logger.success(&format!("Worker spawned and ready: {}", worker.id()));
            worker
        }
        Err(error) => {
            logger.error(&format!("Failed to spawn worker: {error}"));
            return;
        }
    };

    // `spawn` returns once the worker reported `ready`, so work can be sent
    // immediately.
    let execute = Envelope::execute(object(json!({"name": "RunPy"})));
    if let Err(error) = worker.send_message(execute).await {
        logger.error_with("Worker", &format!("execute not sent: {error}"));
    }
    let mut meta = Meta::new();
    meta.insert("some_custom_meta".into(), json!(42));
    let custom = Envelope::new(meta, object(json!({"event": "custom manager message"})))
        .expect("custom metadata must not use x_ keys");
    if let Err(error) = worker.send_message(custom).await {
        logger.error_with("Worker", &format!("custom message not sent: {error}"));
    }

    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    for report in manager.watchdog().report().await {
        logger.info_with(
            "Health",
            &format!(
                "[{:?}] {} (pid {})",
                report.state, report.worker_name, report.pid
            ),
        );
    }

    logger.info("Shutting down...");
    match worker.terminate().await {
        Ok(exit) => logger.info(&format!("Worker stopped: {:?}", exit.reason)),
        Err(error) => logger.warning(&format!("terminate failed: {error}")),
    }
    manager.terminate_all().await;
}
