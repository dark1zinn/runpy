"""Asyncio worker with sequential execution and cooperative cancellation."""

from __future__ import annotations

import asyncio
import sys
from types import TracebackType
from typing import Dict, Optional, Set, Type

from ._protocol import (
    PROTOCOL_VERSION,
    FrameLimits,
    decode_envelope,
    encode_frame,
    error_data,
    invalid_result,
    operation_envelope,
    recv_frame_async,
    stamp,
)
from .runScript import _fail, _parse_extra_args
from .utils import Data, Envelope, ExecutePayload, ExecuteResult, Meta, create_envelope

_MANAGER_OPERATIONS = frozenset({"execute", "cancel", "terminate"})
#: Capabilities this SDK advertises in ``ready``.
_CAPABILITIES = ("requests", "cancel")
#: Custom envelopes that may wait for :meth:`AsyncWorker.handle_envelope`.
CUSTOM_QUEUE_CAPACITY = 64


class _Active:
    """The one request currently executing."""

    def __init__(self, request_id: str) -> None:
        self.request_id = request_id
        self.task: Optional["asyncio.Task[None]"] = None
        self.cancel_requested = False
        #: Set once ``execute`` returned; the request can no longer be cancelled.
        self.finished = False


class AsyncWorker:
    """Base class for an asyncio Python process managed by Runpy.

    Like :class:`runpyrs.Worker`, it runs one ``execute`` request at a time,
    but the request runs as an asyncio task while a separate loop keeps
    reading control messages. ``cancel`` therefore reaches a running request:
    its task is cancelled, and the Manager is told it stopped only after the
    task actually finished. Cancellation is cooperative: a request that
    suppresses :class:`asyncio.CancelledError` keeps running, and its normal
    result is reported instead. A hook that blocks the event loop (CPU work
    without ``await``) also blocks control messages; the Manager then stops
    the whole process when its termination grace ends.

    Custom envelopes are handled sequentially by :meth:`handle_envelope` from
    a queue of :data:`CUSTOM_QUEUE_CAPACITY`; a full queue answers with an
    unsolicited ``error`` of kind ``QueueFull`` instead of blocking control.

    Example:
        .. code-block:: python

            import asyncio
            from runpyrs import AsyncWorker, RunAsyncScript

            class Fetcher(AsyncWorker):
                async def execute(self, data: dict) -> dict:
                    await asyncio.sleep(1)  # cancellable while waiting
                    return {"url": data["url"]}

            if __name__ == "__main__":
                RunAsyncScript(Fetcher)
    """

    def __init__(self, sock: str, name: str, extra: Optional[Dict[str, str]] = None):
        """Store the connection settings; :meth:`run` connects.

        Args:
            sock: Manager-created Unix socket path.
            name: Exact Manager-generated worker identity.
            extra: Parsed ``--key=value`` arguments.

        Raises:
            ValueError: If ``name`` is empty or a Manager-supplied frame limit
                is not a positive integer.
        """
        if not name:
            raise ValueError("Worker ID is required")
        self._limits = FrameLimits.from_env()
        self._sock_path = sock
        self.name = name
        self.extra: Dict[str, str] = extra or {}
        self._writer: Optional[asyncio.StreamWriter] = None
        self._write_lock: Optional[asyncio.Lock] = None
        self._open = False
        self._active: Optional[_Active] = None
        self._tasks: Set["asyncio.Task[None]"] = set()

    async def __aenter__(self) -> "AsyncWorker":
        return self

    async def __aexit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc: Optional[BaseException],
        traceback: Optional[TracebackType],
    ) -> None:
        await self.close()

    # -- Hooks -----------------------------------------------------------------

    async def execute(self, data: ExecutePayload) -> ExecuteResult:
        """Process one ``execute`` request payload.

        Override this coroutine and return a dictionary for ``done.data`` or
        ``None`` for an empty result. Exceptions and non-dictionary results
        become ``error`` envelopes. Await regularly so cancellation and other
        control messages can be processed.
        """
        return None

    async def handle_envelope(self, envelope: Envelope) -> None:
        """Handle one custom Manager envelope without ``meta.x_op``.

        Custom envelopes are handled one at a time, concurrently with a
        running request. An exception becomes an unsolicited ``error``.
        """

    # -- Sending ---------------------------------------------------------------

    async def send(self, data: Data, *, meta: Optional[Meta] = None) -> None:
        """Send an application-defined envelope without ``x_op``.

        Raises:
            TypeError: If ``data`` or ``meta`` is not a dictionary.
            ValueError: For reserved metadata, oversized or non-finite data.
            ConnectionError: If the connection is closed.
        """
        await self._send_envelope(create_envelope(data, meta))

    async def log(
        self, data: Data, *, level: str = "info", meta: Optional[Meta] = None
    ) -> None:
        """Send a structured ``log`` operation; ``level`` replaces ``meta``'s.

        If the socket write fails, one flushed stdout fallback line
        ``[runpy-log-fallback][level=<level>] <data repr>`` is attempted, then
        the original error is re-raised.
        """
        log_meta = dict(meta or {})
        log_meta["level"] = level
        try:
            await self._send_envelope(operation_envelope("log", data, meta=log_meta))
        except OSError:
            try:
                print(f"[runpy-log-fallback][level={level}] {data!r}", flush=True)
            except (OSError, ValueError):
                pass
            raise

    async def close(self) -> None:
        """Cancel running hooks and close the connection; idempotent."""
        self._open = False
        current = asyncio.current_task()
        for task in list(self._tasks):
            if task is not current:
                task.cancel()
        writer, self._writer = self._writer, None
        if writer is not None:
            writer.close()
            try:
                await writer.wait_closed()
            except OSError:
                pass

    async def _send_envelope(self, envelope: Envelope) -> None:
        """Stamp, encode, and write one complete frame under the write lock.

        Encoding happens first, so encoding errors leave the connection
        usable. A failed write may leave a partial frame, so it closes the
        connection.
        """
        frame = encode_frame(
            stamp(envelope, self.name, self._sock_path), self._limits.max_frame_bytes
        )
        if self._write_lock is None or self._writer is None or not self._open:
            raise ConnectionError("Worker connection is closed")
        async with self._write_lock:
            writer = self._writer
            if writer is None or not self._open:
                raise ConnectionError("Worker connection is closed")
            try:
                writer.write(frame)
                await writer.drain()
            except BaseException:
                self._open = False
                writer.close()
                raise

    async def _send_result(self, operation: str, data: Data, request_id: str) -> None:
        await self._send_envelope(
            operation_envelope(operation, data, request_id=request_id)
        )

    # -- Lifecycle -------------------------------------------------------------

    def _spawn(self, coroutine) -> "asyncio.Task[None]":
        task = asyncio.ensure_future(coroutine)
        self._tasks.add(task)
        task.add_done_callback(self._tasks.discard)
        return task

    async def _run_request(self, active: _Active, data: Data) -> None:
        """Execute one request and send exactly one terminal answer for it:
        ``done``, ``error``, or ``cancelled`` with status ``cancelled``.

        Once ``execute`` returned, the request is marked finished
        synchronously, so a later ``cancel`` cannot interrupt the terminal
        write; it is acknowledged with ``already_finished`` after it. A
        ``cancel`` that the request suppressed is acknowledged the same way.
        """
        request_id = active.request_id
        try:
            try:
                result = await self.execute(data)
            except asyncio.CancelledError:
                if not active.cancel_requested:
                    raise  # Shutdown, not a request cancellation.
                active.finished = True
                await self._send_result(
                    "cancelled", {"status": "cancelled"}, request_id
                )
                return
            except Exception as error:
                active.finished = True
                await self._send_result("error", error_data(error), request_id)
            else:
                active.finished = True
                invalid = invalid_result(result, "AsyncWorker.execute")
                if invalid is not None:
                    await self._send_result("error", invalid, request_id)
                else:
                    try:
                        await self._send_result("done", result or {}, request_id)
                    except (TypeError, ValueError) as error:
                        # Encoding failed before any byte was written.
                        await self._send_result("error", error_data(error), request_id)
            if active.cancel_requested:
                await self._send_result(
                    "cancelled", {"status": "already_finished"}, request_id
                )
        finally:
            if self._active is active:
                self._active = None

    async def _cancel(self, request_id: str) -> None:
        """Cancel the active request, or acknowledge one that is not running.

        The acknowledgement for a finished request always follows its
        terminal answer on the wire.
        """
        active = self._active
        if active is not None and active.request_id == request_id:
            if not active.finished:
                if not active.cancel_requested:
                    active.cancel_requested = True
                    assert active.task is not None
                    active.task.cancel()
                return  # The request's task sends the answers.
            if active.task is not None:
                await asyncio.wait({active.task})
        await self._send_result("cancelled", {"status": "already_finished"}, request_id)

    async def _handle_custom(self, queue: "asyncio.Queue[Envelope]") -> None:
        while True:
            envelope = await queue.get()
            try:
                await self.handle_envelope(envelope)
            except Exception as error:
                await self._send_envelope(
                    operation_envelope("error", error_data(error))
                )

    async def run(self) -> None:
        """Connect, announce readiness, and process envelopes until shutdown.

        The first frame is ``ready`` advertising ``requests`` and ``cancel``.
        Returns normally after ``terminate`` or when the Manager closes the
        connection; running hooks are cancelled and the connection is always
        closed first.

        Raises:
            ValueError: For protocol violations, including a second
                ``execute`` while one is running.
            OSError: For socket failures, including a stalled frame.
        """
        reader, writer = await asyncio.open_unix_connection(self._sock_path)
        self._writer = writer
        self._write_lock = asyncio.Lock()
        self._open = True
        custom: "asyncio.Queue[Envelope]" = asyncio.Queue(CUSTOM_QUEUE_CAPACITY)
        custom_task = self._spawn(self._handle_custom(custom))
        try:
            await self._send_envelope(
                operation_envelope(
                    "ready",
                    {
                        "protocol_version": PROTOCOL_VERSION,
                        "capabilities": list(_CAPABILITIES),
                    },
                )
            )
            while self._open:
                payload = await recv_frame_async(reader, self._limits)
                if payload is None:
                    return
                envelope = decode_envelope(payload, _MANAGER_OPERATIONS)
                operation = envelope["meta"].get("x_op")
                if operation == "terminate":
                    return
                if operation == "execute":
                    # A finished request may still be writing its answer;
                    # the Manager only sends the next one after reading it.
                    if self._active is not None and not self._active.finished:
                        raise ValueError(
                            "execute received while a request is still running"
                        )
                    active = _Active(envelope["meta"]["x_req"])
                    active.task = self._spawn(
                        self._run_request(active, envelope["data"])
                    )
                    self._active = active
                elif operation == "cancel":
                    await self._cancel(envelope["meta"]["x_req"])
                else:
                    try:
                        custom.put_nowait(envelope)
                    except asyncio.QueueFull:
                        await self._send_envelope(
                            operation_envelope(
                                "error",
                                {
                                    "kind": "QueueFull",
                                    "message": "custom envelope was not dispatched: "
                                    f"{CUSTOM_QUEUE_CAPACITY} are already waiting",
                                },
                            )
                        )
        finally:
            custom_task.cancel()
            await self.close()


def RunAsyncScript(worker_class: Type[AsyncWorker]) -> None:
    """Instantiate an :class:`AsyncWorker` subclass and run it with asyncio.

    Arguments, diagnostics, and exit statuses match :func:`runpyrs.RunScript`:
    status 0 after ``terminate`` or a Manager close, one stderr diagnostic and
    status 1 for missing arguments, a wrong class, initialization, connection,
    protocol, or socket failures.

    Example:
        .. code-block:: python

            if __name__ == "__main__":
                RunAsyncScript(Fetcher)
    """
    if len(sys.argv) < 3:
        _fail("Error: Socket path and worker ID arguments required")
    if not isinstance(worker_class, type) or not issubclass(worker_class, AsyncWorker):
        name = getattr(worker_class, "__name__", repr(worker_class))
        _fail(f"Configuration error: {name} must inherit from AsyncWorker")
    try:
        worker = worker_class(sys.argv[1], sys.argv[2], _parse_extra_args(sys.argv[3:]))
    except Exception as error:
        _fail(f"Worker initialization failed: {type(error).__name__}: {error}")
    try:
        asyncio.run(worker.run())
    except Exception as error:
        _fail(f"Protocol violation: {type(error).__name__}: {error}")
