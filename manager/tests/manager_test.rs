use runpy::{Manager, Message};

#[tokio::test]
async fn test_spawn_worker() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let venv_path = format!("{}/../worker/.venv", manifest_dir);
    let scripts_path = format!("{}/../worker/src/scripts", manifest_dir);

    let mut manager = Manager::new(&venv_path, &scripts_path);

    // Register a global handler
    manager.on_message(|envelope| {
        println!(
            "[global] Worker '{}' sent: {:?}",
            envelope.worker_id, envelope.message
        );
    });

    // Create and configure a worker
    let mut worker = manager.worker("test");

    worker.on_message(|envelope| {
        println!(
            "[worker] Message: {:?}",
            envelope.message
        );
    });

    // Spawn it
    let worker_id = worker.spawn().await.expect("Failed to spawn worker");
    println!("Spawned worker: {}", worker_id);

    // Give the worker time to start and send READY
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Send an EXECUTE message with the new HTTP-like schema
    let exec_msg = Message::execute(serde_json::json!({
        "html": "<html><title>Test</title></html>"
    }));
    worker.send_message(exec_msg).await.expect("Failed to send message");

    // Wait for response
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    // Check watchdog report
    let reports = manager.dog.report().await;
    for report in &reports {
        println!("Report: {:?}", report);
    }

    // Terminate
    worker.terminate().await.expect("Failed to terminate worker");

    println!("\n✓ Test complete.");
}
