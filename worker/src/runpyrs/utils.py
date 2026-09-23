"""Types and builders for Runpy's bare JSON envelope."""

from __future__ import annotations

from typing import Any, Dict, Literal, Optional, TypedDict


Meta = Dict[str, Any]
"""Developer metadata. Keys beginning with ``x_`` are reserved by Runpy."""

Data = Dict[str, Any]
"""Developer-owned message data."""


class Envelope(TypedDict):
    """The complete value exchanged between a manager and worker."""

    meta: Meta
    data: Data


RunpyOperation = Literal[
    "ready", "execute", "retry", "terminate", "done", "error", "log"
]
"""Operations reserved for Runpy in ``meta["x_op"]``."""

ExecutePayload = Data
"""The direct ``data`` object passed to ``Worker.execute``."""

ExecuteResult = Optional[Data]
"""A direct result object, or ``None`` for an empty completion result."""


def create_envelope(data: Data, meta: Optional[Meta] = None) -> Envelope:
    """Build an application-defined envelope.

    Application metadata cannot use Runpy's reserved ``x_`` namespace.
    Inputs are copied so later caller mutations do not alter the envelope.
    """

    if not isinstance(data, dict):
        raise TypeError("data must be a dictionary")
    if meta is not None and not isinstance(meta, dict):
        raise TypeError("meta must be a dictionary")

    envelope_meta = dict(meta or {})
    reserved = next((key for key in envelope_meta if key.startswith("x_")), None)
    if reserved is not None:
        raise ValueError(f"metadata key '{reserved}' is reserved by Runpy")

    return {"meta": envelope_meta, "data": dict(data)}
