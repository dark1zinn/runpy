use runpy::{Data, Envelope, EnvelopeError, Meta};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};

fn object(value: Value) -> Data {
    value
        .as_object()
        .cloned()
        .expect("test value must be an object")
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
