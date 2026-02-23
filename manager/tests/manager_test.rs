#[tokio::test]
async fn test_spawn_worker() {
    // Almost had a seizure figuring out the realtory paths... until I (Copilot) remembered CARGO_MANIFEST_DIR exists :D
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let venv_path = format!("{}/../py-worker/.venv", manifest_dir);
    let scripts_path = format!("{}/../py-worker/src/scripts", manifest_dir);

    let mut manager = runpy::Runpy::new(&venv_path, &scripts_path);

    let _worker = manager.spawn_worker("test").await.expect("Failed to spawn worker");
    
    println!("\nShutting down...");
}