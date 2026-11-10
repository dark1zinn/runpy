use runpy::{Data, Envelope, EnvelopeError, Meta};
use serde_json::{Value, json};

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
        json!({"meta": {"x_op": "retry"}, "data": {}}),
        json!({"meta": {"x_req": 7}, "data": {}}),
        json!({"meta": {"x_req": "7"}, "data": {}}),
        json!({"meta": {"x_op": "log", "x_req": "7"}, "data": {}}),
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
fn request_ids_are_accepted_as_reserved_strings() {
    let envelope: Envelope = serde_json::from_value(json!({
        "meta": {"x_op": "done", "x_req": "41"},
        "data": {}
    }))
    .unwrap();
    assert_eq!(envelope.meta()["x_req"], "41");
}
