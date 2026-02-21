use serde::{Serialize, Deserialize};
use tokio::net::UnixListener;
use std::process::{Command, Child};

mod protocol;
use protocol::ControlPlane;

#[derive(Serialize)]
pub struct ScrapingRequest {
    html: String,
}

#[derive(Deserialize, Debug)]
pub struct ScrapingResponse {
    pub status: String,
    pub title: String,
    pub links_count: usize,
}

pub struct Runpy {
    _process: Child, // Keep it alive
    pub socket_path: String,
    plane: ControlPlane,
}

impl Runpy {
    pub async fn new(script: &str, sock: &str) -> Self {
        if std::path::Path::new(sock).exists() {
            std::fs::remove_file(sock).expect("Failed to remove existing socket");
        }

        let listener = UnixListener::bind(sock).expect("Failed to remove existing socket");
        let mut plane = ControlPlane::new(listener);
        plane.start().await;
        // tokio::spawn(async move {
        // });
        
        let child = Command::new("./py-worker/.venv/bin/python")
        .arg(script)
        .arg(sock)
        .spawn()
        .expect("Failed to start worker");
        
        // Give the socket a moment to initialize
        std::thread::sleep(std::time::Duration::from_millis(500));

        Self { _process: child, socket_path: sock.to_string(), plane }
    }

    // fn spawn(script: &str, sock: &str) -> Child {
    //     Command::new("./py-worker/.venv/bin/python")
    //         .arg(script)
    //         .arg(sock)
    //         .spawn()
    //         .expect("Failed to start worker")
    // }

    // pub async fn call(&mut self, req: ScrapingRequest) -> ScrapingResponse {
        
    //     let payload = serde_json::to_string(&req).unwrap();
    //     self.plane.listener.write_all(payload.as_bytes()).unwrap();
    //     self.plane.listener.shutdown(std::net::Shutdown::Write).unwrap();
        
    //     let mut response_raw = String::new();
    //     self.plane.listener.read_to_string(&mut response_raw).unwrap();
        
    //     serde_json::from_str(&response_raw).expect("Failed to parse Python response")
    // }
}

#[tokio::main]
async fn main() {
    let _ = Runpy::new("./py-worker/src/scripts/test.py", "./sockets/runpy_rp.sock");
    
    

    // let response = manager.call(req).await;
    // println!("Python says: {:?}", response);
}