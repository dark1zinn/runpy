use runpy::{ControlPlane, Data, Envelope, EnvelopeError, MessageHandler, MessageSender, Meta};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
    mpsc as std_mpsc,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

fn object(value: Value) -> Data {
    value
        .as_object()
        .cloned()
        .expect("test value must be an object")
}

async fn write_frame(stream: &mut UnixStream, payload: &[u8]) {
    stream
        .write_all(&(payload.len() as u64).to_le_bytes())
        .await
        .unwrap();
    stream.write_all(payload).await.unwrap();
    stream.flush().await.unwrap();
}

async fn read_frame(stream: &mut UnixStream) -> Vec<u8> {
    let mut size_buf = [0_u8; 8];
    stream.read_exact(&mut size_buf).await.unwrap();
    let mut payload = vec![0_u8; u64::from_le_bytes(size_buf) as usize];
    stream.read_exact(&mut payload).await.unwrap();
    payload
}

fn custom_envelope() -> Envelope {
    let mut meta = Meta::new();
    meta.insert("some_custom_meta".into(), json!(42));
    Envelope::new(meta, object(json!({"some": "data"}))).unwrap()
}

#[test]
fn serializes_exact_bare_envelope_shape() {
    let serialized = serde_json::to_value(custom_envelope()).unwrap();
    assert_eq!(
        serialized,
        json!({
            "meta": {"some_custom_meta": 42},
            "data": {"some": "data"}
        })
    );
}

#[test]
fn deserializes_heterogeneous_metadata_and_data() {
    let envelope: Envelope = serde_json::from_value(json!({
        "meta": {
            "x_wid": "hsajyh3266",
            "x_spath": "/tmp/some/socket.sock",
            "some_custom_meta": 42
        },
        "data": {"some": "data"}
    }))
    .unwrap();

    assert_eq!(envelope.meta()["some_custom_meta"], 42);
    assert_eq!(envelope.data()["some"], "data");
}

#[test]
fn rejects_invalid_top_level_shapes() {
    for value in [
        json!({"meta": {}}),
        json!({"data": {}}),
        json!({"meta": null, "data": {}}),
        json!({"meta": {}, "data": []}),
        json!({"meta": {}, "data": {}, "extra": true}),
        json!([]),
    ] {
        assert!(
            serde_json::from_value::<Envelope>(value).is_err(),
            "invalid shape was accepted"
        );
    }
}

#[test]
fn rejects_invalid_reserved_metadata() {
    for value in [
        json!({"meta": {"x_custom": true}, "data": {}}),
        json!({"meta": {"x_wid": 42}, "data": {}}),
        json!({"meta": {"x_spath": []}, "data": {}}),
        json!({"meta": {"x_op": 42}, "data": {}}),
        json!({"meta": {"x_op": "unknown"}, "data": {}}),
    ] {
        assert!(
            serde_json::from_value::<Envelope>(value).is_err(),
            "invalid reserved metadata was accepted"
        );
    }
}

#[test]
fn custom_constructor_rejects_reserved_namespace() {
    let mut meta = Meta::new();
    meta.insert("x_wid".into(), json!("spoofed"));

    assert_eq!(
        Envelope::new(meta, Data::new()),
        Err(EnvelopeError::ReservedMetadata("x_wid".into()))
    );
}

#[test]
fn lifecycle_constructors_use_direct_data() {
    let execute = Envelope::execute(object(json!({"task": "scrape"})));
    assert_eq!(execute.meta()["x_op"], "execute");
    assert_eq!(execute.data()["task"], "scrape");

    assert_eq!(Envelope::retry().meta()["x_op"], "retry");
    assert!(Envelope::retry().data().is_empty());
    assert_eq!(Envelope::terminate().meta()["x_op"], "terminate");
    assert!(Envelope::terminate().data().is_empty());
}

#[test]
fn clone_is_independent() {
    let original = custom_envelope();
    let cloned = original.clone();
    assert_eq!(original, cloned);
}

#[tokio::test(flavor = "multi_thread")]
async fn inbound_envelope_gets_trusted_metadata() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("trusted-inbound.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (tx, rx) = std_mpsc::channel();

    let handler: MessageHandler = Arc::new(move |inbound| {
        tx.send(inbound.envelope).unwrap();
    });
    let plane = ControlPlane::new(
        listener,
        "trusted-worker".into(),
        "/tmp/trusted.sock".into(),
        Some(handler),
        None,
    );
    let _sender = plane.start();

    let mut client = UnixStream::connect(&path).await.unwrap();
    let spoofed = serde_json::to_vec(&json!({
        "meta": {
            "x_op": "ready",
            "x_wid": "spoofed",
            "x_spath": "/tmp/spoofed.sock",
            "some_custom_meta": 42
        },
        "data": {"some": "data"}
    }))
    .unwrap();
    write_frame(&mut client, &spoofed).await;

    let received = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(received.meta()["x_wid"], "trusted-worker");
    assert_eq!(received.meta()["x_spath"], "/tmp/trusted.sock");
    assert_eq!(received.meta()["some_custom_meta"], 42);
    assert_eq!(received.data()["some"], "data");
}

#[tokio::test]
async fn outbound_envelope_gets_trusted_metadata() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("trusted-outbound.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let plane = ControlPlane::new(
        listener,
        "trusted-worker".into(),
        "/tmp/trusted.sock".into(),
        None,
        None,
    );
    let sender = plane.start();

    let mut client = UnixStream::connect(&path).await.unwrap();
    sender
        .send(Envelope::execute(object(json!({"task": "scrape"}))))
        .await
        .unwrap();

    let payload = timeout(Duration::from_secs(1), read_frame(&mut client))
        .await
        .unwrap();
    let envelope: Envelope = serde_json::from_slice(&payload).unwrap();
    assert_eq!(envelope.meta()["x_wid"], "trusted-worker");
    assert_eq!(envelope.meta()["x_spath"], "/tmp/trusted.sock");
    assert_eq!(envelope.meta()["x_op"], "execute");
    assert_eq!(envelope.data()["task"], "scrape");
}

#[tokio::test(flavor = "multi_thread")]
async fn mailer_reply_uses_stamped_wire_path() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("mailer.sock");
    let listener = UnixListener::bind(&path).unwrap();

    let handler: MessageHandler = Arc::new(|inbound| {
        inbound
            .mailer
            .send(Envelope::execute(object(json!({"reply": true}))));
    });
    let plane = ControlPlane::new(
        listener,
        "trusted-worker".into(),
        "/tmp/trusted.sock".into(),
        None,
        Some(handler),
    );
    let _sender = plane.start();

    let mut client = UnixStream::connect(&path).await.unwrap();
    let ready = serde_json::to_vec(&json!({
        "meta": {"x_op": "ready"},
        "data": {}
    }))
    .unwrap();
    write_frame(&mut client, &ready).await;

    let payload = timeout(Duration::from_secs(1), read_frame(&mut client))
        .await
        .unwrap();
    let envelope: Envelope = serde_json::from_slice(&payload).unwrap();
    assert_eq!(envelope.meta()["x_wid"], "trusted-worker");
    assert_eq!(envelope.meta()["x_spath"], "/tmp/trusted.sock");
    assert_eq!(envelope.meta()["x_op"], "execute");
    assert_eq!(envelope.data()["reply"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_to_manager_wrong_direction_closes_connection_without_dispatch() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("wrong-inbound.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = calls.clone();
    let handler: MessageHandler = Arc::new(move |_| {
        calls_clone.fetch_add(1, Ordering::SeqCst);
    });
    let plane = ControlPlane::new(
        listener,
        "worker".into(),
        "/tmp/worker.sock".into(),
        Some(handler),
        None,
    );
    let _sender = plane.start();

    let mut client = UnixStream::connect(&path).await.unwrap();
    let invalid = serde_json::to_vec(&Envelope::execute(Data::new())).unwrap();
    write_frame(&mut client, &invalid).await;

    let mut byte = [0_u8; 1];
    let result = timeout(Duration::from_secs(1), client.read_exact(&mut byte))
        .await
        .unwrap();
    assert!(result.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn manager_to_worker_wrong_direction_writes_nothing_and_closes_connection() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("wrong-outbound.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let plane = ControlPlane::new(
        listener,
        "worker".into(),
        "/tmp/worker.sock".into(),
        None,
        None,
    );
    let sender = plane.start();

    let mut client = UnixStream::connect(&path).await.unwrap();
    let ready: Envelope = serde_json::from_value(json!({
        "meta": {"x_op": "ready"},
        "data": {}
    }))
    .unwrap();
    sender.send(ready).await.unwrap();

    let mut size = [0_u8; 8];
    let result = timeout(Duration::from_secs(1), client.read_exact(&mut size))
        .await
        .unwrap();
    assert!(result.is_err());
}

#[tokio::test]
async fn clean_connection_close_is_observed() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("close.sock");
    let listener = UnixListener::bind(&path).unwrap();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut size = [0_u8; 8];
        assert!(stream.read_exact(&mut size).await.is_err());
    });

    let client = UnixStream::connect(&path).await.unwrap();
    drop(client);
    server.await.unwrap();
}

#[tokio::test]
async fn message_sender_delivers_and_clones() {
    let (tx, mut rx) = mpsc::channel::<Envelope>(4);
    let sender = MessageSender::for_testing(tx);
    let cloned = sender.clone();

    sender.send(Envelope::retry()).await.unwrap();
    cloned.send(Envelope::terminate()).await.unwrap();

    assert_eq!(rx.recv().await.unwrap().meta()["x_op"], "retry");
    assert_eq!(rx.recv().await.unwrap().meta()["x_op"], "terminate");
}

#[tokio::test]
async fn message_sender_reports_dropped_receiver() {
    let (tx, rx) = mpsc::channel::<Envelope>(1);
    let sender = MessageSender::for_testing(tx);
    drop(rx);

    let error = sender.send(Envelope::terminate()).await.unwrap_err();
    assert!(error.contains("Failed to send envelope"));
}
