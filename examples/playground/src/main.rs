use runpy::{Manager, Message};


///! Note that this "example" is actually to thinker and test the Runpy functionality during development !
/// 
///! It's not meant to be a polished demo of best practices for using the library — just a quick way to iterate on features and test them out in a real Rust app with Python workers.
#[tokio::main]
async fn main() {
    // Resolve paths relative to the Cargo manifest directory
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let venv_path = format!("{}/worker/.venv", manifest_dir);
    let scripts_path = format!("{}/worker/src/scripts", manifest_dir);

    // ── 1. Create the Manager ─────────────────────────────────────

    let mut manager = Manager::new(&venv_path, &scripts_path);
    println!("✓ Manager initialized");

    // ── 2. (Optional) Global message handler ──────────────────────

    manager.on_message(|envelope| {
        println!(
            "[GLOBAL] Worker '{}' → {:?}",
            envelope.worker_id, envelope.message
        );
    });

    // ── 3. Create, configure, and spawn a worker ──────────────────

    let mut worker = manager.worker("my_script");

    // Set environment variables for the Python process
    worker.env("MY_ENV_VAR", "some_value");

    // Per-worker message handler
    worker.on_message(|envelope| {
        match &envelope.message {
            Message::Ready { message } => {
                println!("READY: {}", message);
                envelope.mailer.send(Message::Execute { 
                    payload: serde_json::json!({ "name": "RunPy" }) 
                });
            },
            Message::Info { message, .. } => println!("INFO: {}", message),
            Message::Done { message, data } => {
                println!("DONE: {} → {}", message, data);
            }
            Message::Error { message, stack_trace } => {
                eprintln!("ERROR: {} ({:?})", message, stack_trace);
            }
            other => println!("OTHER: {:?}", other),
        }
    });

    match worker.spawn().await {
        Ok(id) => println!("✓ Worker spawned: {}", id),
        Err(e) => {
            eprintln!("✗ Failed to spawn worker: {}", e);
            return;
        }
    }

    // ── 4. Let it run, then shut down ────────────────────────────

    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    // Check health via watchdog
    let reports = manager.dog.report().await;
    for r in &reports {
        println!("  [{:?}] {} (pid {})", r.state, r.worker_name, r.pid);
    }

    println!("\n✓ Shutting down...");
    // Manager's Drop automatically kills workers and cleans sockets.
}
