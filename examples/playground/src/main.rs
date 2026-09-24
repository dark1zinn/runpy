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

    let mut manager = Manager::new(&scripts_path);
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

    let mut worker = manager.worker("my_script");
    worker.env("MY_ENV_VAR", "some_value");

    let worker_logger = logger.clone();
    worker.on_message(move |inbound| {
        let operation = inbound.envelope.meta().get("x_op").and_then(Value::as_str);

        match operation {
            Some("ready") => {
                worker_logger.info_with("Worker", "ready");
                inbound
                    .mailer
                    .send(Envelope::execute(object(json!({"name": "RunPy"}))));

                let mut meta = Meta::new();
                meta.insert("some_custom_meta".into(), json!(42));
                inbound.mailer.send(
                    Envelope::new(meta, object(json!({"event": "custom manager message"})))
                        .expect("custom metadata must not use x_ keys"),
                );
            }
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

    match worker.spawn().await {
        Ok(id) => logger.success(&format!("Worker spawned: {id}")),
        Err(error) => {
            logger.error(&format!("Failed to spawn worker: {error}"));
            return;
        }
    }

    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    for report in manager.dog.report().await {
        logger.info_with(
            "Health",
            &format!(
                "[{:?}] {} (pid {})",
                report.state, report.worker_name, report.pid
            ),
        );
    }

    logger.info("Shutting down...");
}
