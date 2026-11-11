"""Public Python SDK for workers managed by the Rust ``runpy`` crate.

Exported types describe the ``{meta, data}`` envelope model. :class:`Worker`
implements the blocking socket lifecycle and :class:`AsyncWorker` the asyncio
one with cooperative cancellation; :func:`RunScript` and
:func:`RunAsyncScript` are the script bootstraps invoked by the Rust Manager.
"""

from ._protocol import FrameTooLargeError
from .async_worker import AsyncWorker, RunAsyncScript
from .runScript import RunScript
from .utils import (
    Data,
    Envelope,
    ExecutePayload,
    ExecuteResult,
    Meta,
    RunpyOperation,
    create_envelope,
)
from .worker import Worker

__all__ = [
    "AsyncWorker",
    "Data",
    "Envelope",
    "ExecutePayload",
    "ExecuteResult",
    "FrameTooLargeError",
    "Meta",
    "RunAsyncScript",
    "RunScript",
    "RunpyOperation",
    "Worker",
    "create_envelope",
]
