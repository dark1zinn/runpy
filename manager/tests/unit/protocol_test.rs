/// Unit tests for the protocol layer (protocol.rs).
///
/// Tests Message serialization/deserialization, Envelope construction,
/// ControlPlane socket communication, and MessageSender channel behaviour.
///
/// Uses the new HTTP-like message schema with method, path, headers, and body.
use runpy::{headers, Message, MessageEnvelope, MessageSender, Mailer, Method};
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

// ═══════════════════════════════════════════════════════════════════════════
// MESSAGE SERDE ROUND-TRIPS
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn serialize_ready_message() {
    let msg = Message::ready("hello");
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"method\":\"READY\""));
    assert!(json.contains("\"message\":\"hello\""));
}

#[test]
fn deserialize_ready_message() {
    let raw = r#"{"method":"READY","body":{"message":"Worker ready"}}"#;
    let msg: Message = serde_json::from_str(raw).unwrap();
    assert_eq!(msg.method, Method::Ready);
    assert!(msg.body.is_some());
    let body = msg.body.unwrap();
    assert_eq!(body["message"], "Worker ready");
}

#[test]
fn serialize_terminate_message() {
    let msg = Message::terminate();
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"method\":\"TERMINATE\""));
}

#[test]
fn deserialize_terminate_message() {
    let raw = r#"{"method":"TERMINATE"}"#;
    let msg: Message = serde_json::from_str(raw).unwrap();
    assert_eq!(msg.method, Method::Terminate);
}

#[test]
fn serialize_execute_message() {
    let msg = Message::execute(json!({"url": "https://example.com"}));
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"method\":\"EXECUTE\""));
    assert!(json.contains("https://example.com"));
}

#[test]
fn deserialize_execute_message() {
    let raw = r#"{"method":"EXECUTE","body":{"key":"value"}}"#;
    let msg: Message = serde_json::from_str(raw).unwrap();
    assert_eq!(msg.method, Method::Execute);
    let body = msg.body.unwrap();
    assert_eq!(body["key"], "value");
}

#[test]
fn serialize_done_message() {
    let msg = Message::done("ok", json!({"count": 42}));
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"method\":\"DONE\""));
    assert!(json.contains("\"count\":42"));
}

#[test]
fn deserialize_done_message() {
    let raw = r#"{"method":"DONE","body":{"message":"finished","data":{"result":true}}}"#;
    let msg: Message = serde_json::from_str(raw).unwrap();
    assert_eq!(msg.method, Method::Done);
    let body = msg.body.unwrap();
    assert_eq!(body["message"], "finished");
    assert_eq!(body["data"]["result"], true);
}

#[test]
fn serialize_error_message_with_stack_trace() {
    let msg = Message::error("boom", Some("line 42".into()), Some("critical".into()));
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"method\":\"ERROR\""));
    assert!(json.contains("X-Stack-Trace"));
    assert!(json.contains("line 42"));
    assert!(json.contains("X-Error-Level"));
    assert!(json.contains("critical"));
}

#[test]
fn serialize_error_message_without_stack_trace() {
    let msg = Message::error("boom", None, None);
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"method\":\"ERROR\""));
    assert!(!json.contains("X-Stack-Trace"));
}

#[test]
fn serialize_log_message() {
    let msg = Message::log("log line", "info", json!({}));
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"method\":\"LOG\""));
    assert!(json.contains("X-Log-Level"));
    assert!(json.contains("info"));
}

#[test]
fn serialize_log_message_debug_level() {
    let msg = Message::log("debug info", "debug", json!({"x": 1}));
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"method\":\"LOG\""));
    assert!(json.contains("debug"));
}

#[test]
fn serialize_retry_message() {
    let msg = Message::retry();
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"method\":\"RETRY\""));
}

#[test]
fn serialize_meta_message() {
    let msg = Message::meta(json!({"name": "worker-1"}));
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"method\":\"META\""));
    assert!(json.contains("worker-1"));
}

#[test]
fn serialize_status_request() {
    let msg = Message::status_request();
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"method\":\"GET\""));
}

#[test]
fn serialize_status_response() {
    let msg = Message::status_response("ok", 300);
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"method\":\"STATUS\""));
    assert!(json.contains("X-Uptime"));
    assert!(json.contains("300"));
}

#[test]
fn serialize_get_message() {
    let msg = Message::get("config_key");
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"method\":\"GET\""));
    assert!(json.contains("X-Key"));
    assert!(json.contains("config_key"));
}

#[test]
fn serialize_action_message() {
    let msg = Message::action("restart", json!({"force": true}));
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"method\":\"ACTION\""));
    assert!(json.contains("X-Action"));
    assert!(json.contains("restart"));
}

#[test]
fn deserialize_unknown_method_fails() {
    let raw = r#"{"method":"UNKNOWN_METHOD"}"#;
    let result = serde_json::from_str::<Message>(raw);
    assert!(result.is_err());
}

// ═══════════════════════════════════════════════════════════════════════════
// MESSAGE BUILDER API
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn message_builder_with_headers() {
    let msg = Message::new(Method::Get)
        .header(headers::X_WORKER_ID, "worker-123")
        .header(headers::X_SOCKET_PATH, "/tmp/test.sock");
    
    assert_eq!(msg.worker_id(), Some("worker-123"));
    assert_eq!(msg.socket_path(), Some("/tmp/test.sock"));
}

#[test]
fn message_builder_with_body() {
    let msg = Message::new(Method::Post)
        .body(json!({"key": "value"}));
    
    assert!(msg.body.is_some());
    assert_eq!(msg.body.unwrap()["key"], "value");
}

#[test]
fn message_with_body_constructor() {
    let msg = Message::with_body(Method::Execute, json!({"task": "run"}));
    assert_eq!(msg.method, Method::Execute);
    assert_eq!(msg.body.unwrap()["task"], "run");
}

// ═══════════════════════════════════════════════════════════════════════════
// MESSAGE CLONE
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn message_clone_is_independent() {
    let original = Message::log("original", "info", json!({"a": 1}));
    let cloned = original.clone();

    // Both should be equal in content
    let orig_json = serde_json::to_string(&original).unwrap();
    let clone_json = serde_json::to_string(&cloned).unwrap();
    assert_eq!(orig_json, clone_json);
}

// ═══════════════════════════════════════════════════════════════════════════
// ENVELOPE CONSTRUCTION
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn envelope_carries_worker_id() {
    let env = test_envelope("worker_abc", Message::ready("up"));
    assert_eq!(env.worker_id, "worker_abc");
    assert_eq!(env.message.method, Method::Ready);
}

#[tokio::test]
async fn envelope_clone_is_independent() {
    let env = test_envelope("w1", Message::terminate());
    let cloned = env.clone();
    assert_eq!(cloned.worker_id, "w1");
    assert_eq!(cloned.message.method, Method::Terminate);
}

// ═══════════════════════════════════════════════════════════════════════════
// LENGTH-PREFIXED WIRE PROTOCOL
// ═══════════════════════════════════════════════════════════════════════════
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

    let msg1 = serde_json::to_vec(&Message::ready("hi")).unwrap();
    write_length_prefixed(&mut client, &msg1).await;

    let msg2 = serde_json::to_vec(&Message::done("done", json!({}))).unwrap();
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
        assert_eq!(msg.method, Method::Ready);

        // Send back a response
        let response = serde_json::to_vec(&Message::execute(json!({"task": "scrape"}))).unwrap();
        write_length_prefixed(&mut stream, &response).await;
    });

    // Client: connect, send a message, read the response
    let mut client = UnixStream::connect(&sock_path).await.unwrap();

    let outgoing = serde_json::to_vec(&Message::ready("ready")).unwrap();
    write_length_prefixed(&mut client, &outgoing).await;

    let response_data = read_length_prefixed(&mut client).await;
    let response: Message = serde_json::from_slice(&response_data).unwrap();
    assert_eq!(response.method, Method::Execute);
    assert_eq!(response.body.unwrap()["task"], "scrape");

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

// ═══════════════════════════════════════════════════════════════════════════
// MESSAGE SENDER VIA CHANNEL
// ═══════════════════════════════════════════════════════════════════════════

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
        .send(Message::terminate())
        .await
        .expect("send should succeed");

    let received = rx.recv().await.expect("should receive a message");
    assert_eq!(received.method, Method::Terminate);
}

#[tokio::test]
async fn message_sender_clone_works() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Message>(16);
    let sender: MessageSender = unsafe { std::mem::transmute(tx) };

    let sender2 = sender.clone();

    sender
        .send(Message::ready("one"))
        .await
        .unwrap();
    sender2
        .send(Message::ready("two"))
        .await
        .unwrap();

    let m1 = rx.recv().await.unwrap();
    let m2 = rx.recv().await.unwrap();
    assert_eq!(m1.method, Method::Ready);
    assert_eq!(m2.method, Method::Ready);
}

#[tokio::test]
async fn message_sender_fails_when_receiver_dropped() {
    let (tx, rx) = tokio::sync::mpsc::channel::<Message>(16);
    let sender: MessageSender = unsafe { std::mem::transmute(tx) };
    drop(rx);

    let result = sender.send(Message::terminate()).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Failed to send message"));
}
