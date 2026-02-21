
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
    Debug { message: String, data: Value },
    Terminate,
    Ready { message: String },
    Retry,
    Meta { data: Value },
    Execute { payload: Value },
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
                Err(e) => {
                    match e.kind() {
                        std::io::ErrorKind::UnexpectedEof => {
                            println!("Connection closed by peer");
                        }
                        std::io::ErrorKind::ConnectionReset => {
                            eprintln!("Connection reset by peer");
                        }
                        std::io::ErrorKind::PermissionDenied => {
                            eprintln!("Permission denied on socket operation");
                        }
                        std::io::ErrorKind::BrokenPipe => {
                            eprintln!("Broken pipe - peer closed unexpectedly");
                        }
                        _ => {
                            eprintln!("Unknown error: {} (kind: {:?})", e, e.kind());
                        }
                    }
                    break;
                }
            }

            let message_size = u64::from_le_bytes(size_buf) as usize;

            // Read exactly `message_size` bytes
            let mut message_buf = vec![0u8; message_size];
            match stream.read_exact(&mut message_buf).await {
                Ok(_) => {},
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    eprintln!("Connection closed by peer while reading message body");
                    break;
                }
                Err(e) => {
                    eprintln!("Error reading message body: {} (kind: {:?})", e, e.kind());
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
                    eprintln!("JSON parse error: {} \n  Raw: {}", e, message_str);
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
                
                let req_msg = Message::Execute {
                    payload: serde_json::to_value(req).unwrap()
                };
                if let Err(e) = Self::send_message(stream, &req_msg).await {
                    eprintln!("Failed to send scraping request: {}", e);
                }
            },
            Message::Info { message, .. } => println!("LOG: {}", message),
            Message::Debug { message, data } => println!("DEBUG: {} \n  With data: {}", message, data),
            Message::Error { message, stack_trace } => {
                eprintln!("Worker ERROR: {} \n  Stack trace: {:?}", message, stack_trace);
                let req: Message = Message::Terminate;
                if let Err(e) = Self::send_message(stream, &req).await {
                    eprintln!("Failed to send termination message: {}", e);
                }
            },
            Message::Done { message, data } => {
                println!("DONE: {} with data: {}", message, data);
                
                let req: Message = Message::Terminate;
                
                if let Err(e) = Self::send_message(stream, &req).await {
                    eprintln!("Failed to send termination message: {}", e);
                }
            },
            _ => println!("Received message type: {:?}", msg),
        }
    }
    
    // Send length prefix + payload
    pub async fn send_message(stream: &mut tokio::net::UnixStream, msg: &Message) -> Result<(), Box<dyn std::error::Error>> {
        let payload = serde_json::to_string(msg)?;
        let size = payload.len() as u64;
        
        stream.write_all(&size.to_le_bytes()).await?;
        stream.write_all(payload.as_bytes()).await?;
        Ok(())
    }
}
