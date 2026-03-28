"""Typing utilities for the runpyrs package.

Re-exported from the top-level package so users can write::

    from runpyrs import Worker, ExecuteResult, Envelope

These types improve editor auto-complete, static analysis, and serve as
living documentation for the Runpy wire protocol.
"""

from __future__ import annotations

from typing import Any, Dict, Literal, Optional, TypedDict, Union


# ── Message type literals ───────────────────────────────────────────────

InternalMessageType = Literal["EXECUTE", "TERMINATE", "META", "RETRY"]
"""Message types handled internally by the Worker base class."""

BuiltinResponseType = Literal["READY", "DONE", "ERROR", "DEBUG"]
"""Response types the Worker emits back to the Rust manager."""

MessageType = Union[InternalMessageType, BuiltinResponseType, str]
"""Any message type that can appear on the wire (including custom ones)."""


# ── Inbound (Rust → Python) ────────────────────────────────────────────

class Envelope(TypedDict, total=False):
    """Shape of every message received from the Rust manager.

    ``type`` is always present; the remaining fields are optional
    depending on the message kind.
    """

    type: str
    message: str
    data: Dict[str, Any]
    payload: Dict[str, Any]


class MetaData(TypedDict, total=False):
    """Contents of ``data`` in a META message."""

    name: str


# ── Outbound (Python → Rust) ───────────────────────────────────────────

class OutboundMessage(TypedDict):
    """Shape of every message sent from the worker to the Rust manager."""

    type: str
    message: str
    data: Dict[str, Any]


# ── User-facing type aliases ───────────────────────────────────────────

ExecutePayload = Dict[str, Any]
"""Type of the *payload* dict passed to ``Worker.execute()``."""

ExecuteResult = Optional[Dict[str, Any]]
"""Return type of ``Worker.execute()``.

Return a dict to have it sent back as a ``DONE`` message, or ``None``
for a result-less acknowledgement.
"""

RequestData = Envelope
"""Alias — the dict handed to ``Worker.handle_request()``."""

HandleRequestResult = None
"""Return type of ``Worker.handle_request()`` (always None)."""

SendData = Optional[Dict[str, Any]]
"""Type accepted for the *data* parameter of ``Worker.send()``."""
