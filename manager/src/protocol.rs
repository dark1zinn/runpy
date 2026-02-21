
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
    Done { message: String, data: Value },
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
        
        loop {
            let (mut stream, _) = self.listener.accept().await.unwrap();
            tokio::spawn(async move {
                Self::handle_connection(&mut stream).await;
            });
        }
    }

    async fn handle_connection(stream: &mut tokio::net::UnixStream) {
        loop {
            // Read the length prefix (8 bytes for u64)
            let mut size_buf = [0u8; 8];
            match stream.read_exact(&mut size_buf).await {
                Ok(_) => {},
                Err(_) => {
                    eprintln!("Connection closed or read error");
                    break;
                }
            }

            let message_size = u64::from_le_bytes(size_buf) as usize;

            // Read exactly `message_size` bytes
            let mut message_buf = vec![0u8; message_size];
            match stream.read_exact(&mut message_buf).await {
                Ok(_) => {},
                Err(e) => {
                    eprintln!("Failed to read message: {}", e);
                    break;
                }
            }

            let message_str = String::from_utf8_lossy(&message_buf);

            // Parse and handle the message
            match serde_json::from_str::<Message>(&message_str) {
                Ok(msg) => {
                    Self::handle_message(msg, stream).await;
                }
                Err(e) => {
                    eprintln!("JSON parse error: {} | Raw: {}", e, message_str);
                }
            }
        }
    }

    async fn handle_message(msg: Message, stream: &mut tokio::net::UnixStream) {
        match msg {
            Message::Ready { message } => {
                println!("READY: {}", message);
                
                let req = ScrapingRequest {
                    html: "<html><title>Hello from Rust!</title><body><a href='#'>Link</a></body></html>".into()
                };
                
                let payload = serde_json::to_string(&req).unwrap();
                let size = payload.len() as u64;
                
                // Send length prefix + payload
                if let Err(e) = stream.write_all(&size.to_le_bytes()).await {
                    eprintln!("Failed to write size: {}", e);
                    return;
                }
                if let Err(e) = stream.write_all(payload.as_bytes()).await {
                    eprintln!("Failed to write payload: {}", e);
                    return;
                }
            },
            Message::Info { message, .. } => println!("LOG: {}", message),
            Message::Error { message, .. } => eprintln!("PY ERROR: {}", message),
            // TODO: THE PYHTON PROCESS BECOMES A ZOMBIE AFTER SENDING THIS MESSAGE, NEED TO HANDLE THE TERMINATION OF THE PROCESS!!!
            Message::Done { message, data } => { println!("DONE: {} with data: {}", message, data) },
            _ => println!("Received message type: {:?}", msg),
        }
    }
}
