"""Typed envelope model shared by :class:`runpyrs.Worker` helpers."""

from __future__ import annotations

from typing import Any, Dict, Literal, Optional, TypedDict


#: Developer-owned metadata. Every key beginning with ``x_`` is reserved.
Meta = Dict[str, Any]

#: Developer-owned message body.
Data = Dict[str, Any]


class Envelope(TypedDict):
    """Complete JSON value exchanged between a Manager and worker."""

    #: Application metadata plus trusted Runpy routing fields on the wire.
    meta: Meta
    #: Application-owned message body.
    data: Data


#: Operation names reserved for Runpy in ``meta["x_op"]``.
RunpyOperation = Literal[
    "ready", "execute", "retry", "terminate", "done", "error", "log"
]

#: Direct ``data`` object supplied to :meth:`runpyrs.Worker.execute`.
ExecutePayload = Data

#: Dictionary returned by ``execute``, or ``None`` for an empty ``done`` body.
ExecuteResult = Optional[Data]


def create_envelope(data: Data, meta: Optional[Meta] = None) -> Envelope:
    """Build an application-defined envelope with no internal operation.

    ``data`` and ``meta`` must be dictionaries. Both are shallow-copied so
    later top-level caller mutations do not alter the envelope. Application
    metadata may contain arbitrary JSON-compatible values, but keys beginning
    with ``x_`` are reserved by Runpy.

    Args:
        data: Application-owned message body.
        meta: Optional application metadata.

    Returns:
        A new ``{"meta": ..., "data": ...}`` dictionary.

    Raises:
        TypeError: If either supplied object is not a dictionary.
        ValueError: If an application metadata key begins with ``x_``.

    Example:
        >>> create_envelope(
        ...     {"task": "parse"},
        ...     {"correlation_id": 42},
        ... )
        {'meta': {'correlation_id': 42}, 'data': {'task': 'parse'}}
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
