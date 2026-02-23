use chrono::Local;
use tokio::net::UnixListener;
use rand::{distributions::Alphanumeric, Rng};
use std::{path::PathBuf, process::Child};

use crate::protocol::ControlPlane;

#[derive(Debug)]
pub struct WorkerIdentity {
    pub name: String,
    pub sock_file: String,
}

impl WorkerIdentity {
    pub fn new(script: &str) -> Self {
        let ts = Local::now().format("%d%m%Y-%H%M").to_string();
        let rnd: String = rand::thread_rng().sample_iter(&Alphanumeric).take(4).map(char::from).collect();
        let name = format!("{}_{}_{}", script, ts, rnd);
        
        Self {
            sock_file: format!("rp_{}.sock", name),
            name,
        }
    }
}

#[derive(Debug)]
pub struct ActiveWorker {
    pub child: Child,
    pub identity: WorkerIdentity,
}

impl ActiveWorker {
    pub fn new(venv: &PathBuf, sock_path: &PathBuf, script: &str) -> Self {
        let identity = WorkerIdentity::new(script);
        
        let sock = sock_path.join(&identity.sock_file);
        println!("Starting worker '{}' with socket at '{}'", identity.name, sock.display());
        
        // Ensure clean socket start
        let _ = std::fs::remove_file(&sock);

        let listener = UnixListener::bind(sock).expect("Failed to bind to socket");
        let mut plane = ControlPlane::new(listener);

        tokio::spawn(async move {
            plane.start().await;
        });

        let py_executable = if cfg!(windows) {
            venv.join("Scripts/python.exe")
        } else {
            venv.join("bin/python")
        };

        let child = std::process::Command::new(py_executable)
            .arg(script.to_string()+".py")
            .arg(sock_path.join(&identity.sock_file))
            .spawn()
            .expect("Failed to start worker");

        Self { child, identity }
    }
}

impl Drop for ActiveWorker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.identity.sock_file);
    }
}