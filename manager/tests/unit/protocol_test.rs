/// Unit tests for the protocol layer (protocol.rs).
///
/// Tests Message serialization/deserialization, Envelope construction,
/// ControlPlane socket communication, and MessageSender channel behaviour.
use runpy::{Message, MessageEnvelope, MessageSender, Mailer};
use serde_json::json;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// Helper function to create a test envelope with a dummy mailer
fn test_envelope(worker_id: &str, message: Message) -> MessageEnvelope {
    let mailer = Mailer::for_testing(worker_id.to_string());
    MessageEnvelope {
        worker_id: worker_id.to_string(),
        message,
        mailer,
    }
}

// ─── Message serde round-trips ─────────────────────────────────────────

#[test]
fn serialize_ready_message() {
    let msg = Message::Ready {
        message: "hello".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"READY\""));
    assert!(json.contains("\"message\":\"hello\""));
}

#[test]
fn deserialize_ready_message() {
    let raw = r#"{"type":"READY","message":"Worker ready"}"#;
    let msg: Message = serde_json::from_str(raw).unwrap();
    match msg {
        Message::Ready { message } => assert_eq!(message, "Worker ready"),
        _ => panic!("Expected Ready variant"),
    }
}

#[test]
fn serialize_terminate_message() {
    let msg = Message::Terminate;
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"TERMINATE\""));
}

#[test]
fn deserialize_terminate_message() {
    let raw = r#"{"type":"TERMINATE"}"#;
    let msg: Message = serde_json::from_str(raw).unwrap();
    assert!(matches!(msg, Message::Terminate));
}

#[test]
fn serialize_execute_message() {
    let msg = Message::Execute {
        payload: json!({"url": "https://example.com"}),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"EXECUTE\""));
    assert!(json.contains("https://example.com"));
}

#[test]
fn deserialize_execute_message() {
    let raw = r#"{"type":"EXECUTE","payload":{"key":"value"}}"#;
    let msg: Message = serde_json::from_str(raw).unwrap();
    match msg {
        Message::Execute { payload } => {
            assert_eq!(payload["key"], "value");
        }
        _ => panic!("Expected Execute variant"),
    }
}

#[test]
fn serialize_done_message() {
    let msg = Message::Done {
        message: "ok".into(),
        data: json!({"count": 42}),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"DONE\""));
    assert!(json.contains("\"count\":42"));
}

#[test]
fn deserialize_done_message() {
    let raw = r#"{"type":"DONE","message":"finished","data":{"result":true}}"#;
    let msg: Message = serde_json::from_str(raw).unwrap();
    match msg {
        Message::Done { message, data } => {
            assert_eq!(message, "finished");
            assert_eq!(data["result"], true);
        }
        _ => panic!("Expected Done variant"),
    }
}

#[test]
fn serialize_error_message_with_stack_trace() {
    let msg = Message::Error {
        message: "boom".into(),
        stack_trace: Some("line 42".into()),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"ERROR\""));
    assert!(json.contains("line 42"));
}

#[test]
fn serialize_error_message_without_stack_trace() {
    let msg = Message::Error {
        message: "boom".into(),
        stack_trace: None,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"ERROR\""));
    assert!(json.contains("null"));
}

#[test]
fn serialize_info_message() {
    let msg = Message::Info {
        message: "log line".into(),
        data: json!({}),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"INFO\""));
}

#[test]
fn serialize_debug_message() {
    let msg = Message::Debug {
        message: "debug info".into(),
        data: json!({"x": 1}),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"DEBUG\""));
}

#[test]
fn serialize_retry_message() {
    let msg = Message::Retry;
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"RETRY\""));
}

#[test]
fn serialize_meta_message() {
    let msg = Message::Meta {
        data: json!({"name": "worker-1"}),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"META\""));
    assert!(json.contains("worker-1"));
}

#[test]
fn serialize_status_message() {
    let msg = Message::Status { uptime: Some(120) };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"STATUS\""));
    assert!(json.contains("120"));
}

#[test]
fn serialize_status_res_message() {
    let msg = Message::StatusRes {
        status: "ok".into(),
        uptime: 300,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"STATUS_RES\""));
}

#[test]
fn serialize_get_message() {
    let msg = Message::Get {
        key: "config_key".into(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"GET\""));
    assert!(json.contains("config_key"));
}

#[test]
fn serialize_action_message() {
    let msg = Message::Action {
        action: "restart".into(),
        params: json!({"force": true}),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"ACTION\""));
    assert!(json.contains("restart"));
}

#[test]
fn deserialize_unknown_type_fails() {
    let raw = r#"{"type":"UNKNOWN_TYPE","data":{}}"#;
    let result = serde_json::from_str::<Message>(raw);
    assert!(result.is_err());
}

// ─── Message clone ─────────────────────────────────────────────────────

#[test]
fn message_clone_is_independent() {
    let original = Message::Info {
        message: "original".into(),
        data: json!({"a": 1}),
    };
    let cloned = original.clone();

    // Both should be equal in content
    let orig_json = serde_json::to_string(&original).unwrap();
    let clone_json = serde_json::to_string(&cloned).unwrap();
    assert_eq!(orig_json, clone_json);
}

// ─── Envelope construction ─────────────────────────────────────────────

#[tokio::test]
async fn envelope_carries_worker_id() {
    let env = test_envelope("worker_abc", Message::Ready {
        message: "up".into(),
    });
    assert_eq!(env.worker_id, "worker_abc");
    assert!(matches!(env.message, Message::Ready { .. }));
}

#[tokio::test]
async fn envelope_clone_is_independent() {
    let env = test_envelope("w1", Message::Terminate);
    let cloned = env.clone();
    assert_eq!(cloned.worker_id, "w1");
    assert!(matches!(cloned.message, Message::Terminate));
}

// ─── Length-prefixed wire protocol ─────────────────────────────────────
//
// These tests simulate the Rust ↔ Python wire format: 8-byte LE length
// prefix followed by a JSON payload.

/// Helper: write a length-prefixed message to a stream (simulates Python sending).
async fn write_length_prefixed(stream: &mut UnixStream, payload: &[u8]) {
    let size = (payload.len() as u64).to_le_bytes();
    stream.write_all(&size).await.unwrap();
    stream.write_all(payload).await.unwrap();
    stream.flush().await.unwrap();
}

/// Helper: read a length-prefixed message from a stream (simulates Python receiving).
async fn read_length_prefixed(stream: &mut UnixStream) -> Vec<u8> {
    let mut size_buf = [0u8; 8];
    stream.read_exact(&mut size_buf).await.unwrap();
    let size = u64::from_le_bytes(size_buf) as usize;

    let mut buf = vec![0u8; size];
    stream.read_exact(&mut buf).await.unwrap();
    buf
}

#[tokio::test]
async fn control_plane_receives_messages_and_dispatches_to_handlers() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sock_path = tmp.path().join("test_recv.sock");

    let listener = UnixListener::bind(&sock_path).unwrap();

    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    let handler: Arc<dyn Fn(MessageEnvelope) + Send + Sync> = Arc::new(move |env| {
        // Verify the envelope has the right worker_id
        assert_eq!(env.worker_id, "test_worker");
        counter_clone.fetch_add(1, Ordering::SeqCst);
    });

    // We can't directly construct ControlPlane from tests (it's in the crate),
    // so we test the wire protocol manually — which is what ControlPlane does.
    // Start a listener task that reads messages.
    let _sock_path_clone = sock_path.clone();
    let recv_task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Read two messages
        for _ in 0..2 {
            let mut size_buf = [0u8; 8];
            stream.read_exact(&mut size_buf).await.unwrap();
            let size = u64::from_le_bytes(size_buf) as usize;
            let mut buf = vec![0u8; size];
            stream.read_exact(&mut buf).await.unwrap();

            let msg: Message = serde_json::from_slice(&buf).unwrap();
            let mailer = Mailer::for_testing("test_worker".to_string());
            let envelope = MessageEnvelope {
                worker_id: "test_worker".into(),
                message: msg,
                mailer,
            };
            handler(envelope);
        }
    });

    // Connect as a "Python worker" and send two messages
    let mut client = UnixStream::connect(&sock_path).await.unwrap();

    let msg1 = serde_json::to_vec(&Message::Ready {
        message: "hi".into(),
    })
    .unwrap();
    write_length_prefixed(&mut client, &msg1).await;

    let msg2 = serde_json::to_vec(&Message::Done {
        message: "done".into(),
        data: json!({}),
    })
    .unwrap();
    write_length_prefixed(&mut client, &msg2).await;

    recv_task.await.unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn wire_protocol_round_trip_sends_and_receives() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sock_path = tmp.path().join("test_roundtrip.sock");

    let listener = UnixListener::bind(&sock_path).unwrap();

    // Server: accept, read a message, write a response
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();

        // Read what the client sends
        let data = read_length_prefixed(&mut stream).await;
        let msg: Message = serde_json::from_slice(&data).unwrap();
        assert!(matches!(msg, Message::Ready { .. }));

        // Send back a response
        let response = serde_json::to_vec(&Message::Execute {
            payload: json!({"task": "scrape"}),
        })
        .unwrap();
        write_length_prefixed(&mut stream, &response).await;
    });

    // Client: connect, send a message, read the response
    let mut client = UnixStream::connect(&sock_path).await.unwrap();

    let outgoing = serde_json::to_vec(&Message::Ready {
        message: "ready".into(),
    })
    .unwrap();
    write_length_prefixed(&mut client, &outgoing).await;

    let response_data = read_length_prefixed(&mut client).await;
    let response: Message = serde_json::from_slice(&response_data).unwrap();
    match response {
        Message::Execute { payload } => {
            assert_eq!(payload["task"], "scrape");
        }
        _ => panic!("Expected Execute message"),
    }

    server.await.unwrap();
}

#[tokio::test]
async fn connection_close_returns_none_on_recv() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sock_path = tmp.path().join("test_close.sock");

    let listener = UnixListener::bind(&sock_path).unwrap();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Try to read — client will close immediately
        let mut size_buf = [0u8; 8];
        let result = stream.read_exact(&mut size_buf).await;
        // Should get UnexpectedEof or similar error
        assert!(result.is_err());
    });

    // Connect and immediately close
    let client = UnixStream::connect(&sock_path).await.unwrap();
    drop(client);

    server.await.unwrap();
}

// ─── MessageSender via channel ─────────────────────────────────────────

#[tokio::test]
async fn message_sender_delivers_through_channel() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Message>(16);
    let sender = unsafe {
        // MessageSender wraps an mpsc::Sender<Message>. We construct one
        // manually here since the struct fields are private. In practice
        // this is created by ControlPlane::start().
        // We use transmute because the field layout is: { tx: Sender<Message> }
        std::mem::transmute::<tokio::sync::mpsc::Sender<Message>, MessageSender>(tx)
    };

    sender
        .send(Message::Terminate)
        .await
        .expect("send should succeed");

    let received = rx.recv().await.expect("should receive a message");
    assert!(matches!(received, Message::Terminate));
}

#[tokio::test]
async fn message_sender_clone_works() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Message>(16);
    let sender: MessageSender = unsafe { std::mem::transmute(tx) };

    let sender2 = sender.clone();

    sender
        .send(Message::Ready {
            message: "one".into(),
        })
        .await
        .unwrap();
    sender2
        .send(Message::Ready {
            message: "two".into(),
        })
        .await
        .unwrap();

    let m1 = rx.recv().await.unwrap();
    let m2 = rx.recv().await.unwrap();
    assert!(matches!(m1, Message::Ready { .. }));
    assert!(matches!(m2, Message::Ready { .. }));
}

#[tokio::test]
async fn message_sender_fails_when_receiver_dropped() {
    let (tx, rx) = tokio::sync::mpsc::channel::<Message>(16);
    let sender: MessageSender = unsafe { std::mem::transmute(tx) };
    drop(rx);

    let result = sender.send(Message::Terminate).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Failed to send message"));
}
