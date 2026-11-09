"""Private wire framing shared by Runpy worker implementations.

Every frame is an 8-byte little-endian body length followed by a UTF-8 JSON
body. The Manager propagates its limits through the environment so both peers
enforce the same maximum body size and partial-frame deadline.
"""

from __future__ import annotations

import json
import os
import select
import socket
import struct
import time
from dataclasses import dataclass
from typing import Mapping, Optional

DEFAULT_MAX_FRAME_BYTES = 8 * 1024 * 1024
DEFAULT_FRAME_TIMEOUT_MS = 30_000
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
        ValueError: If ``value`` contains NaN or infinite numbers.
        TypeError: If ``value`` is not JSON serializable.
    """
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


def decode_body(payload: bytes) -> object:
    """Decode a UTF-8 JSON body, rejecting NaN and infinite numbers."""
    return json.loads(payload.decode("utf-8"), parse_constant=_reject_constant)


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
