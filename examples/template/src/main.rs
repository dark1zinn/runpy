use runpy::Runpy;

#[tokio::main]
async fn main() {
    // This is some shennanigans to get the correct paths to the virtual environment and scripts directory
    // Applicable only on development environment, i think... 
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    // Initialize Runpy with the path to your Python virtual environment
    // and the directory containing your Python scripts
    let venv_path = format!("{}/worker/.venv", manifest_dir);
    let scripts_path = format!("{}/worker/src/scripts", manifest_dir);

    let mut runpy = Runpy::new(&venv_path, &scripts_path);
    println!("✓ Runpy initialized");

    // Spawn a worker for a specific Python script
    // The script name should NOT include the .py extension
    match runpy.spawn_worker("my_script").await {
        Ok(worker_id) => {
            println!("✓ Worker spawned with ID: {}", worker_id);
        }
        Err(e) => {
            eprintln!("✗ Failed to spawn worker: {}", e);
            return;
        }
    }

    // Spawn another worker
    match runpy.spawn_worker("another_script").await {
        Ok(worker_id) => {
            println!("✓ Another worker spawned with ID: {}", worker_id);
        }
        Err(e) => {
            eprintln!("✗ Failed to spawn second worker: {}", e);
        }
    }

    // Let workers run for a bit
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    // Runpy's Drop impl will automatically:
    // - Terminate all active workers
    // - Clean up socket files
    println!("\n✓ Shutting down...");
}

// Example configuration for your project:
//
// Your project structure should look like:
//   project/
//   ├── venv/                 # Python virtual environment
//   │   └── bin/python        # Python executable
//   ├── scripts/              # Python scripts directory
//   │   ├── my_script.py
//   │   └── another_script.py
//   ├── Cargo.toml
//   └── src/
//       └── main.rs           # This file
//
// Update the paths in main() to match your setup!
