"""Private wire framing shared by Runpy worker implementations.

Every frame is an 8-byte little-endian body length followed by a UTF-8 JSON
body. The Manager propagates its limits through the environment so both peers
enforce the same maximum body size and partial-frame deadline.
"""

from __future__ import annotations

import asyncio
import json
import os
import select
import socket
import struct
import time
from dataclasses import dataclass
from typing import FrozenSet, Mapping, Optional

from .utils import Data, Envelope, Meta, create_envelope

DEFAULT_MAX_FRAME_BYTES = 8 * 1024 * 1024
DEFAULT_FRAME_TIMEOUT_MS = 30_000
PROTOCOL_VERSION = 1
#: Deepest container nesting accepted, including the top-level envelope.
MAX_JSON_DEPTH = 64

#: Every operation the protocol defines.
RUNPY_OPERATIONS = frozenset(
    {"ready", "execute", "cancel", "terminate", "done", "error", "cancelled", "log"}
)
#: Metadata keys Runpy reserves; values must be strings.
RESERVED_KEYS = frozenset({"x_wid", "x_spath", "x_op", "x_req"})
#: Manager operations that address one request.
_REQUEST_OPERATIONS = frozenset({"execute", "cancel"})
ENV_MAX_FRAME_BYTES = "RUNPY_MAX_FRAME_BYTES"
ENV_FRAME_TIMEOUT_MS = "RUNPY_FRAME_TIMEOUT_MS"

_HEADER = struct.Struct("<Q")
_RECV_CHUNK = 64 * 1024
_ENCODER = json.JSONEncoder(separators=(",", ":"), allow_nan=False)


class FrameTooLargeError(ValueError):
    """Raised when a frame body exceeds the negotiated maximum size."""


def _positive_int(environ: Mapping[str, str], name: str, default: int) -> int:
    raw = environ.get(name)
    if raw is None:
        return default
    try:
        value = int(raw)
    except ValueError:
        raise ValueError(f"{name} must be a positive integer, got {raw!r}") from None
    if value <= 0:
        raise ValueError(f"{name} must be a positive integer, got {raw!r}")
    return value


@dataclass(frozen=True)
class FrameLimits:
    """Maximum body size in bytes and partial-frame deadline in seconds."""

    max_frame_bytes: int = DEFAULT_MAX_FRAME_BYTES
    frame_timeout: float = DEFAULT_FRAME_TIMEOUT_MS / 1000

    @classmethod
    def from_env(cls, environ: Optional[Mapping[str, str]] = None) -> "FrameLimits":
        """Read limits supplied by the Manager, defaulting missing values.

        Raises:
            ValueError: If a supplied value is not a positive integer.
        """
        source = os.environ if environ is None else environ
        return cls(
            max_frame_bytes=_positive_int(
                source, ENV_MAX_FRAME_BYTES, DEFAULT_MAX_FRAME_BYTES
            ),
            frame_timeout=_positive_int(
                source, ENV_FRAME_TIMEOUT_MS, DEFAULT_FRAME_TIMEOUT_MS
            )
            / 1000,
        )


def encode_frame(value: object, max_frame_bytes: int) -> bytes:
    """Serialize ``value`` into one complete frame without passing the limit.

    Serialization stops as soon as the body would exceed ``max_frame_bytes``
    rather than materializing an arbitrarily large string first.

    Raises:
        FrameTooLargeError: If the body would exceed ``max_frame_bytes``.
        ValueError: If ``value`` contains NaN or infinite numbers or nests
            deeper than :data:`MAX_JSON_DEPTH`.
        TypeError: If ``value`` is not JSON serializable.
    """
    if value_depth(value) > MAX_JSON_DEPTH:
        raise ValueError(
            f"Envelope nesting exceeds the maximum depth of {MAX_JSON_DEPTH}"
        )
    frame = bytearray(_HEADER.size)
    for chunk in _ENCODER.iterencode(value):
        encoded = chunk.encode("utf-8")
        if len(frame) - _HEADER.size + len(encoded) > max_frame_bytes:
            raise FrameTooLargeError(
                f"Envelope exceeds the maximum frame size of {max_frame_bytes} bytes"
            )
        frame += encoded
    _HEADER.pack_into(frame, 0, len(frame) - _HEADER.size)
    return bytes(frame)


def _reject_constant(name: str) -> object:
    raise ValueError(f"Non-finite JSON number '{name}' is not allowed")


def _reject_duplicates(pairs: list) -> dict:
    result: dict = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"Duplicate JSON object key '{key}'")
        result[key] = value
    return result


def check_depth(payload: bytes) -> None:
    """Reject bodies nested deeper than :data:`MAX_JSON_DEPTH` in one scan.

    Brackets inside strings, including escaped quotes, are ignored, so the
    check runs before any recursive parsing.
    """
    depth = 0
    in_string = False
    escaped = False
    for byte in payload:
        if in_string:
            if escaped:
                escaped = False
            elif byte == 0x5C:  # backslash
                escaped = True
            elif byte == 0x22:  # quote
                in_string = False
        elif byte == 0x22:
            in_string = True
        elif byte in (0x7B, 0x5B):  # { [
            depth += 1
            if depth > MAX_JSON_DEPTH:
                raise ValueError(
                    f"JSON nesting exceeds the maximum depth of {MAX_JSON_DEPTH}"
                )
        elif byte in (0x7D, 0x5D):  # } ]
            depth = max(depth - 1, 0)


def value_depth(value: object) -> int:
    """Container nesting of ``value``, computed without recursion."""
    deepest = 0
    pending = [(value, 1)]
    while pending:
        item, level = pending.pop()
        if isinstance(item, dict):
            deepest = max(deepest, level)
            pending.extend((child, level + 1) for child in item.values())
        elif isinstance(item, (list, tuple)):
            deepest = max(deepest, level)
            pending.extend((child, level + 1) for child in item)
    return deepest


def decode_body(payload: bytes) -> object:
    """Decode a UTF-8 JSON body under the strict profile.

    Rejects nesting deeper than :data:`MAX_JSON_DEPTH`, duplicate object keys
    at any level, and NaN or infinite numbers, all with ``ValueError``.
    """
    check_depth(payload)
    return json.loads(
        payload.decode("utf-8"),
        parse_constant=_reject_constant,
        object_pairs_hook=_reject_duplicates,
    )


def _recv_until(stream: socket.socket, size: int, deadline: float) -> bytes:
    received = bytearray()
    while len(received) < size:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("Frame did not complete before the frame timeout")
        ready, _, _ = select.select([stream], [], [], remaining)
        if not ready:
            raise TimeoutError("Frame did not complete before the frame timeout")
        chunk = stream.recv(min(size - len(received), _RECV_CHUNK))
        if not chunk:
            raise ConnectionError("Connection closed during a frame")
        received += chunk
    return bytes(received)


def recv_frame(stream: socket.socket, limits: FrameLimits) -> Optional[bytes]:
    """Read one frame body from a blocking socket.

    Waiting for the first byte is unbounded and a clean close before it
    returns ``None``. Once the first byte arrives, the rest of the frame must
    complete within ``limits.frame_timeout``. The declared length is checked
    before reading the body.

    Raises:
        FrameTooLargeError: If the declared body exceeds the limit.
        ValueError: If the frame declares an empty body.
        TimeoutError: If the frame stalls past the deadline.
        ConnectionError: If the peer closes mid-frame.
    """
    first = stream.recv(1)
    if not first:
        return None
    deadline = time.monotonic() + limits.frame_timeout
    header = first + _recv_until(stream, _HEADER.size - 1, deadline)
    size = _HEADER.unpack(header)[0]
    if size == 0:
        raise ValueError("Frame declares an empty body")
    if size > limits.max_frame_bytes:
        raise FrameTooLargeError(
            f"Frame body of {size} bytes exceeds the {limits.max_frame_bytes}-byte limit"
        )
    return _recv_until(stream, size, deadline)


async def recv_frame_async(
    reader: asyncio.StreamReader, limits: FrameLimits
) -> Optional[bytes]:
    """Asynchronous :func:`recv_frame` for an asyncio stream, same contract."""
    first = await reader.read(1)
    if not first:
        return None

    async def rest() -> bytes:
        try:
            header = first + await reader.readexactly(_HEADER.size - 1)
            size = _HEADER.unpack(header)[0]
            if size == 0:
                raise ValueError("Frame declares an empty body")
            if size > limits.max_frame_bytes:
                raise FrameTooLargeError(
                    f"Frame body of {size} bytes exceeds the {limits.max_frame_bytes}-byte limit"
                )
            return await reader.readexactly(size)
        except asyncio.IncompleteReadError as error:
            raise ConnectionError("Connection closed during a frame") from error

    try:
        return await asyncio.wait_for(rest(), limits.frame_timeout)
    except asyncio.TimeoutError as error:
        raise TimeoutError("Frame did not complete before the frame timeout") from error


def decode_envelope(payload: bytes, manager_operations: FrozenSet[str]) -> Envelope:
    """Decode and validate one Manager-to-worker frame body.

    Raises:
        ValueError: For invalid UTF-8 or JSON, a wrong envelope shape,
            invalid reserved metadata, or an operation outside
            ``manager_operations``.
    """
    try:
        decoded = decode_body(payload)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError(f"Invalid JSON envelope: {error}") from error
    return validate_inbound(decoded, manager_operations)


def validate_inbound(value: object, manager_operations: FrozenSet[str]) -> Envelope:
    """Validate the exact envelope shape and Manager operation boundary.

    Custom envelopes omit ``x_op``. ``execute`` and ``cancel`` must carry a
    ``x_req`` request ID, and no other Manager operation may carry one.
    """
    if not isinstance(value, dict) or set(value) != {"meta", "data"}:
        raise ValueError("Envelope must contain exactly 'meta' and 'data'")

    meta = value["meta"]
    data = value["data"]
    if not isinstance(meta, dict) or not isinstance(data, dict):
        raise ValueError("Envelope 'meta' and 'data' must be objects")

    for key, item in meta.items():
        if not key.startswith("x_"):
            continue
        if key not in RESERVED_KEYS:
            raise ValueError(f"Unknown Runpy metadata key '{key}'")
        if not isinstance(item, str):
            raise ValueError(f"Runpy metadata '{key}' must be a string")

    operation = meta.get("x_op")
    if operation is not None:
        if operation not in RUNPY_OPERATIONS:
            raise ValueError(f"Unknown Runpy operation '{operation}'")
        if operation not in manager_operations:
            raise ValueError(
                f"Runpy operation '{operation}' is invalid for manager-to-worker envelopes"
            )
    requires_id = operation in _REQUEST_OPERATIONS
    if requires_id and not meta.get("x_req"):
        raise ValueError(f"{operation} must carry a 'x_req' request ID")
    if "x_req" in meta and not requires_id:
        raise ValueError("Only execute and cancel envelopes may carry 'x_req'")

    return {"meta": dict(meta), "data": dict(data)}


def error_data(error: BaseException) -> Data:
    """Structured error body; an empty message reports the type name."""
    kind = type(error).__name__
    return {"kind": kind, "message": str(error) or kind}


def operation_envelope(
    operation: str,
    data: Data,
    *,
    meta: Optional[Meta] = None,
    request_id: Optional[str] = None,
) -> Envelope:
    """Build a worker-to-Manager operation envelope."""
    envelope = create_envelope(data, meta)
    envelope["meta"]["x_op"] = operation
    if request_id is not None:
        envelope["meta"]["x_req"] = request_id
    return envelope


def stamp(envelope: Envelope, name: str, sock_path: str) -> Envelope:
    """Copy ``envelope`` with this connection's trusted identity fields."""
    meta = dict(envelope["meta"])
    meta["x_wid"] = name
    meta["x_spath"] = sock_path
    return {"meta": meta, "data": dict(envelope["data"])}


def invalid_result(result: object, hook: str) -> Optional[Data]:
    """Error data when an ``execute`` result is not a dictionary or None."""
    if result is None or isinstance(result, dict):
        return None
    return {"kind": "TypeError", "message": f"{hook} must return a dictionary or None"}
