
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::UnixListener};

use crate::ScrapingRequest;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Message {
    Status { uptime: Option<u64> },
    StatusRes { status: String, uptime: u64 },
    Info { message: String, data: Value },
    Error { message: String, stack_trace: Option<String> },
    Get { key: String },
    Action { action: String, params: Value },
    Done { message: String },
    Terminate,
    Ready { message: String },
}

pub struct ControlPlane {
    pub listener: UnixListener,
}

impl ControlPlane {
    pub fn new(listener: UnixListener) -> Self {
        Self { listener }
    }

    pub async fn start(&mut self) {
        println!("Control plane started at {:?}", self.listener.local_addr().unwrap());
        
        // self.listener.set_nonblocking(true).expect("Failed to set non-blocking");
        loop {
            let (mut socket, _) = self.listener.accept().await.unwrap();
            tokio::spawn(async move {
                    // let mut buf = [0; 4096];
                    // let n = socket.read(&mut buf).await.unwrap();
                    let mut response_raw = String::new();
                    socket.read_to_string(&mut response_raw).await.unwrap();
                    let msg: Message = serde_json::from_str(&response_raw).unwrap();
                    
                    match msg {
                        Message::Ready { message } => {
                            println!("READY: {}", message);
            
                            let req = ScrapingRequest {
                                html: "<html><title>Hello from Rust!</title><body><a href='#'>Link</a></body></html>".into()
                            };
                            
                            let payload = serde_json::to_string(&req).unwrap();
                            socket.write_all(payload.as_bytes()).await.unwrap();
                            socket.shutdown().await.unwrap();
                            
                            let mut response_raw = String::new();
                            socket.read_to_string(&mut response_raw).await.unwrap();
                            
                            let response: Message = serde_json::from_str(&response_raw).expect("Failed to parse Python response");
                            println!("Python says: {:?}", response);
                        },
                        Message::Info { message, .. } => println!("LOG: {}", message),
                        Message::Error { message, .. } => eprintln!("PY ERROR: {}", message),
                        Message::Done { .. } => {
                            println!("Worker finished. Cleaning up...");
                            // Logic to kill process here
                        },
                        _ => println!("Received other message type: {:?}", msg),
                    }
                });
            }
        }
    }
    // match self.listener.accept() {
        //     Ok((mut socket, _)) => {
            //         // ... rest of your code
            
            //         let msg: Message = serde_json::from_str(&response_raw).unwrap();
                    // match msg {
                    //     Message::Info { message, .. } => println!("LOG: {}", message),
                    //     Message::Error { message, .. } => eprintln!("PY ERROR: {}", message),
                    //     Message::Done { .. } => {
                    //         println!("Worker finished. Cleaning up...");
                    //         // Logic to kill process here
                    //     },
                    //     _ => println!("Received other message type: {:?}", msg),
                    // }
            
            //     }
            //     Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            //         std::thread::sleep(std::time::Duration::from_millis(100));
            //         continue;
            //     }
            //     Err(e) => panic!("Connection error: {}", e),
            // }

// pub async fn start_control_plane(stream: &UnixStream) {

//     print!("Control plane started at {}", stream);
    
//     loop {
//         let (mut socket, _) = stream.accept().await.unwrap();
//         tokio::spawn(async move {
//         });
//     }
// }
