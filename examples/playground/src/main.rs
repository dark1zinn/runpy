use runpy::{Manager, Message, Method};


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

    // Per-worker message handler using the new HTTP-like protocol
    worker.on_message(|envelope| {
        let msg = &envelope.message;
        let body = msg.body.as_ref();
        
        match msg.method {
            Method::Ready => {
                let message = body
                    .and_then(|b| b.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no message)");
                println!("READY: {}", message);
                
                // Send EXECUTE with payload
                envelope.mailer.send(Message::execute(
                    serde_json::json!({ "name": "RunPy" })
                ));
            }
            Method::Log => {
                let message = body
                    .and_then(|b| b.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no message)");
                let level = msg.get_header("X-Log-Level").unwrap_or("info");
                println!("LOG [{}]: {}", level, message);
            }
            Method::Done => {
                let message = body
                    .and_then(|b| b.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no message)");
                let data = body
                    .and_then(|b| b.get("data"))
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                println!("DONE: {} → {}", message, data);
            }
            Method::Error => {
                let message = body
                    .and_then(|b| b.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no message)");
                let stack_trace = msg.get_header("X-Stack-Trace");
                let error_level = msg.get_header("X-Error-Level").unwrap_or("unknown");
                eprintln!("ERROR [{}]: {} ({:?})", error_level, message, stack_trace);
            }
            _ => println!("OTHER: {:?}", msg),
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
