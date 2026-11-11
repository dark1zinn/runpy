"""Strict JSON profile shared with the Rust Manager."""

import json

import pytest
from runpyrs._protocol import MAX_JSON_DEPTH, decode_body, encode_frame


def nested(levels: int) -> bytes:
    return ("[" * levels + "]" * levels).encode()


def test_depth_limit_is_inclusive():
    assert decode_body(nested(MAX_JSON_DEPTH)) is not None
    with pytest.raises(ValueError, match="maximum depth"):
        decode_body(nested(MAX_JSON_DEPTH + 1))


def test_brackets_in_strings_and_escapes_do_not_count():
    body = json.dumps({"a": "[" * 100 + '"' + "{" * 100, "b": "\\"}).encode()
    assert decode_body(body)["b"] == "\\"


@pytest.mark.parametrize(
    "body",
    [
        '{"meta": {}, "meta": {}, "data": {}}',
        '{"meta": {"k": 1, "k": 2}, "data": {}}',
        '{"meta": {}, "data": {"deep": [{"x": 1, "x": 1}]}}',
        '{"meta": {}, "data": {"\u00e9": 1, "\\u00e9": 2}}',
    ],
)
def test_duplicate_keys_are_rejected_at_any_level(body):
    with pytest.raises(ValueError, match="Duplicate"):
        decode_body(body.encode())


def test_distinct_unicode_names_are_different_keys():
    decoded = decode_body(b'{"\\u00e9": 1, "e\\u0301": 2}')
    assert len(decoded) == 2


def test_outbound_nesting_is_bounded():
    value: object = 1
    # Envelope (1) + data (2) + 62 nested lists reaches the limit.
    for _ in range(MAX_JSON_DEPTH - 2):
        value = [value]
    assert encode_frame({"meta": {}, "data": {"v": value}}, 1 << 20)
    with pytest.raises(ValueError, match="maximum depth"):
        encode_frame({"meta": {}, "data": {"v": [value]}}, 1 << 20)


def test_self_referencing_values_are_rejected_instead_of_looping():
    cyclic: list = []
    cyclic.append(cyclic)
    with pytest.raises(ValueError, match="maximum depth"):
        encode_frame({"meta": {}, "data": {"v": cyclic}}, 1 << 20)
