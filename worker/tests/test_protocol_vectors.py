"""Shared protocol vectors and generated framing properties."""

import asyncio
import json
import socket
import threading
from pathlib import Path

import pytest
from hypothesis import given, settings
from hypothesis import strategies as st

from runpyrs._protocol import (
    MAX_JSON_DEPTH,
    FrameLimits,
    decode_body,
    decode_envelope,
    encode_frame,
    recv_frame,
    recv_frame_async,
)

DOCUMENT = json.loads(
    (
        Path(__file__).resolve().parents[2] / "tests" / "protocol_vectors.json"
    ).read_text()
)
VECTORS = DOCUMENT["vectors"]
SYNC_OPERATIONS = frozenset({"execute", "terminate"})
ASYNC_OPERATIONS = frozenset({"execute", "cancel", "terminate"})


def body(vector: dict) -> bytes:
    if "depth" in vector:
        nesting = vector["depth"] - 2
        return f'{{"meta":{{}},"data":{{"v":{"[" * nesting}{"]" * nesting}}}}}'.encode()
    if "body_hex" in vector:
        return bytes.fromhex(vector["body_hex"])
    return vector["body"].encode()


def accepts(payload: bytes, operations: frozenset) -> bool:
    try:
        decode_envelope(payload, operations)
    except (ValueError, UnicodeDecodeError):
        return False
    return True


@pytest.mark.parametrize(
    "vector",
    [vector for vector in VECTORS if vector["direction"] == "manager_to_worker"],
    ids=lambda vector: vector["name"],
)
@pytest.mark.parametrize("operations", [SYNC_OPERATIONS, ASYNC_OPERATIONS])
def test_workers_accept_exactly_the_shared_manager_vectors(vector, operations):
    assert accepts(body(vector), operations) is vector["valid"]


def read_complete_stream(stream: bytes) -> bool:
    """Whether both readers yield one body from ``stream`` followed by EOF."""
    left, right = socket.socketpair()
    left.sendall(stream)
    left.close()
    try:
        sync_ok = recv_frame(right, FrameLimits()) is not None
    except (ValueError, ConnectionError, TimeoutError):
        sync_ok = False
    finally:
        right.close()

    async def read_async() -> bool:
        reader = asyncio.StreamReader()
        reader.feed_data(stream)
        reader.feed_eof()
        try:
            return await recv_frame_async(reader, FrameLimits()) is not None
        except (ValueError, ConnectionError, TimeoutError):
            return False

    async_ok = asyncio.run(read_async())
    assert sync_ok is async_ok
    return sync_ok


@pytest.mark.parametrize("frame", DOCUMENT["frames"], ids=lambda frame: frame["name"])
def test_frame_readers_accept_exactly_the_shared_frames(frame):
    assert read_complete_stream(bytes.fromhex(frame["frame_hex"])) is frame["valid"]


# Bounded JSON values within the strict profile: finite numbers, unique keys
# (dicts), and shallow nesting.
json_values = st.recursive(
    st.none()
    | st.booleans()
    | st.integers(min_value=-(2**63), max_value=2**64 - 1)
    | st.floats(allow_nan=False, allow_infinity=False)
    | st.text(),
    lambda children: (
        st.lists(children, max_size=4)
        | st.dictionaries(st.text(), children, max_size=4)
    ),
    max_leaves=40,
)


@settings(max_examples=200, derandomize=True)
@given(st.dictionaries(st.text(), json_values, max_size=5))
def test_encoded_envelopes_decode_to_the_same_value(data):
    envelope = {"meta": {}, "data": data}
    frame = encode_frame(envelope, 1 << 20)
    assert decode_body(frame[8:]) == envelope


@settings(max_examples=50, derandomize=True, deadline=None)
@given(
    st.dictionaries(st.text(), json_values, max_size=5),
    st.lists(st.integers(min_value=1, max_value=17), min_size=1, max_size=40),
)
def test_frames_survive_arbitrary_segmentation(data, cuts):
    envelope = {"meta": {}, "data": data}
    frame = encode_frame(envelope, 1 << 20)
    left, right = socket.socketpair()

    def send_segmented() -> None:
        position = 0
        for cut in cuts * (len(frame) // max(sum(cuts), 1) + 1):
            if position >= len(frame):
                break
            left.sendall(frame[position : position + cut])
            position += cut
        left.sendall(frame[position:])

    sender = threading.Thread(target=send_segmented)
    sender.start()
    try:
        payload = recv_frame(right, FrameLimits())
        assert payload is not None
        assert decode_body(payload) == envelope
    finally:
        sender.join(timeout=5)
        left.close()
        right.close()


def test_generated_depth_limit_matches_the_vectors():
    deep = [vector for vector in VECTORS if "depth" in vector]
    assert {vector["depth"] for vector in deep} == {MAX_JSON_DEPTH, MAX_JSON_DEPTH + 1}
