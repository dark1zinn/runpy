"""Public Python SDK for workers managed by the Rust ``runpy`` crate.

Exported types describe the ``{meta, data}`` envelope model. :class:`Worker`
implements the blocking socket lifecycle and :class:`AsyncWorker` the asyncio
one with cooperative cancellation; :func:`RunScript` and
:func:`RunAsyncScript` are the script bootstraps invoked by the Rust Manager.
"""

from ._protocol import FrameTooLargeError
from .async_worker import AsyncWorker, RunAsyncScript
from .worker import Worker
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

__all__ = [
    "Worker",
    "RunScript",
    "AsyncWorker",
    "RunAsyncScript",
    "Envelope",
    "Meta",
    "Data",
    "RunpyOperation",
    "create_envelope",
    "ExecutePayload",
    "ExecuteResult",
    "FrameTooLargeError",
]
