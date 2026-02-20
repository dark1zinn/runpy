use serde::{Serialize, Deserialize};
use std::os::unix::net::UnixStream;
use std::io::{Write, Read};
use std::process::{Command, Child};

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

pub struct PythonManager {
    _process: Child, // Keep it alive
    pub socket_path: String,
}

impl PythonManager {
    pub fn new(script: &str, sock: &str) -> Self {
        let child = Command::new("./py-worker/.venv/bin/python")
            .arg(script)
            .arg(sock)
            .spawn()
            .expect("Failed to start worker");
        
        // Give the socket a moment to initialize
        std::thread::sleep(std::time::Duration::from_millis(500));

        Self { _process: child, socket_path: sock.to_string() }
    }

    pub fn call(&self, req: ScrapingRequest) -> ScrapingResponse {
        let mut stream = UnixStream::connect(&self.socket_path).expect("Connect failed");
        
        let payload = serde_json::to_string(&req).unwrap();
        stream.write_all(payload.as_bytes()).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();

        let mut response_raw = String::new();
        stream.read_to_string(&mut response_raw).unwrap();
        
        serde_json::from_str(&response_raw).expect("Failed to parse Python response")
    }
}

fn main() {
    let manager = PythonManager::new("py-worker/src/scripts/test.py", "/tmp/rust_py.sock");
    
    let req = ScrapingRequest {
        html: "<html><title>Hello from Rust!</title><body><a href='#'>Link</a></body></html>".into()
    };

    let response = manager.call(req);
    println!("Python says: {:?}", response);
}