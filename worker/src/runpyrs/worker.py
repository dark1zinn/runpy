"""Managed worker connection, protocol validation, and dispatch lifecycle."""

from __future__ import annotations

import socket
import threading
from types import TracebackType
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    # Annotation-only: `typing.Self` needs Python 3.11, and the SDK has no
    # runtime dependencies.
    from typing_extensions import Self

from ._protocol import (
    PROTOCOL_VERSION,
    FrameLimits,
    decode_envelope,
    encode_frame,
    error_data,
    invalid_result,
    operation_envelope,
    recv_frame,
    stamp,
)
from .utils import (
    Data,
    Envelope,
    ExecutePayload,
    ExecuteResult,
    Meta,
    create_envelope,
)

_MANAGER_OPERATIONS = frozenset({"execute", "terminate"})
#: Capabilities this SDK advertises in ``ready``.
_CAPABILITIES = ("requests",)


class Worker:
    """Base class for a Python process managed by Runpy.

    The Rust Manager supplies the Unix socket path, trusted worker ID, and
    optional ``--key=value`` arguments. Subclasses normally override
    :meth:`execute` for managed requests and :meth:`handle_envelope` for custom
    messages.

    Attributes:
        name: Trusted Manager-generated worker identity.
        extra: Parsed application ``--key=value`` arguments.
        stream: Connected Unix socket owned by the protocol lifecycle.

    Example:
        .. code-block:: python

            from runpyrs import RunScript, Worker

            class Parser(Worker):
                def execute(self, data: dict) -> dict:
                    return {"parsed": data["text"]}

            if __name__ == "__main__":
                RunScript(Parser)
    """

    def __init__(
        self,
        sock: str,
        name: str,
        extra: dict[str, str] | None = None,
    ):
        """Connect to the Manager.

        The ``ready`` envelope is sent by :meth:`run`, after the subclass has
        finished constructing, so the Manager never sends work to a worker
        that is still initializing.

        Args:
            sock: Manager-created Unix socket path.
            name: Exact Manager-generated worker identity.
            extra: Parsed ``--key=value`` arguments.

        Raises:
            ValueError: If ``name`` is empty or a Manager-supplied frame limit
                in ``RUNPY_MAX_FRAME_BYTES``/``RUNPY_FRAME_TIMEOUT_MS`` is not
                a positive integer.
            OSError: If socket creation or connection fails. The socket is
                closed before the error propagates.
        """
        if not name:
            raise ValueError("Worker ID is required")

        self._limits = FrameLimits.from_env()
        self.__ok = False
        self.__send_lock = threading.Lock()
        self.__sock_path = sock
        self.name = name
        self.extra: dict[str, str] = extra or {}

        self.stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            self.stream.connect(self.__sock_path)
            self.__ok = True
        except BaseException:
            self.close()
            raise

    def __enter__(self) -> Self:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self.close()

    def close(self) -> None:
        """Close the connection; safe to call repeatedly and from any thread.

        A receive loop blocked in another thread observes end-of-stream and
        returns. Later sends raise ``ConnectionError``.
        """
        self.__ok = False
        try:
            self.stream.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.stream.close()

    def _send_bytes(self, frame: bytes) -> None:
        """Write one complete pre-encoded frame atomically.

        A lock keeps frames from different threads from interleaving. A failed
        write may have sent part of a frame, so it closes the connection and
        every later send raises ``ConnectionError`` instead of writing onto a
        corrupted stream.
        """
        with self.__send_lock:
            if not self.__ok:
                raise ConnectionError("Worker connection is closed")
            try:
                self.stream.sendall(frame)
            except BaseException:
                self.close()
                raise

    def _send_envelope(self, envelope: Envelope) -> None:
        """Copy, trust-stamp, serialize, and send one complete envelope.

        Caller-provided ``x_wid`` and ``x_spath`` values are overwritten with
        this connection's Manager-supplied identity and socket path. Encoding
        happens before any byte is written, so an oversized or non-finite
        envelope raises ``ValueError`` and leaves the connection usable.
        """
        stamped = stamp(envelope, self.name, self.__sock_path)
        self._send_bytes(encode_frame(stamped, self._limits.max_frame_bytes))

    def _send_operation(
        self,
        operation: str,
        data: Data,
        *,
        meta: Meta | None = None,
    ) -> None:
        """Build and send one Python-to-Manager operation envelope."""
        self._send_envelope(operation_envelope(operation, data, meta=meta))

    def send(self, data: Data, *, meta: Meta | None = None) -> None:
        """Send an application-defined envelope without ``x_op``.

        Args:
            data: Application-owned dictionary body.
            meta: Optional application metadata without ``x_`` keys.

        Raises:
            TypeError: If ``data`` or ``meta`` is not a dictionary.
            ValueError: If metadata uses Runpy's reserved ``x_`` namespace.
            OSError: If the socket write fails.

        Example:
            ``self.send({"accepted": True}, meta={"request_id": 7})``
        """

        self._send_envelope(create_envelope(data, meta))

    def log(
        self,
        data: Data,
        *,
        level: str = "info",
        meta: Meta | None = None,
    ) -> None:
        """Send a structured ``log`` operation.

        ``level`` always replaces a same-named metadata value. If the socket
        operation raises ``OSError``, one flushed stdout fallback is attempted:
        ``[runpy-log-fallback][level=<level>] <data repr>``. Fallback
        ``OSError`` and closed-stdout ``ValueError`` are suppressed, then the
        original transport error is re-raised. Serialization and metadata
        errors do not use the fallback.

        Args:
            data: Structured log dictionary.
            level: Developer-selected severity string.
            meta: Optional non-reserved metadata.

        Example:
            ``self.log({"message": "started"}, level="info")``
        """

        log_meta = dict(meta or {})
        log_meta["level"] = level
        try:
            self._send_operation("log", data, meta=log_meta)
        except OSError:
            try:
                print(
                    f"[runpy-log-fallback][level={level}] {data!r}",
                    flush=True,
                )
            except (OSError, ValueError):
                pass
            raise

    def handle_envelope(self, envelope: Envelope) -> None:
        """Handle one custom Manager envelope without ``meta.x_op``.

        Override this hook for application-defined messages. An exception is
        converted to an ``error`` envelope by the receive loop.
        """

    def execute(self, data: ExecutePayload) -> ExecuteResult:
        """Process one ``execute`` request payload.

        Override this hook and return a dictionary for ``done.data`` or
        ``None`` for an empty result. Exceptions and non-dictionary results are
        converted to ``error`` envelopes; the receive loop remains active.
        """

        return None

    def _recv_envelope(self) -> Envelope | None:
        """Read, decode, and validate one Manager-to-worker frame.

        The frame uses an 8-byte little-endian payload size followed by UTF-8
        JSON, bounded by the Manager-supplied size and partial-frame deadline.
        Invalid size, encoding, JSON, shape, metadata, or operation direction
        raises ``ValueError``; a stalled frame raises ``TimeoutError``. This
        worker does not support ``cancel``, so it is a protocol violation.
        """
        payload = recv_frame(self.stream, self._limits)
        if payload is None:
            return None
        return decode_envelope(payload, _MANAGER_OPERATIONS)

    def _send_result(self, operation: str, data: Data, request_id: str) -> None:
        """Send ``done`` or ``error`` correlated with ``request_id``."""
        self._send_envelope(operation_envelope(operation, data, request_id=request_id))

    def _complete_execution(self, request_id: str, payload: Data) -> None:
        """Run one request and reply with ``done`` or ``error`` for it.

        An exception, an invalid result type, and a result that cannot be
        encoded produce structured ``error`` envelopes carrying the same
        request ID. ``SystemExit``, ``KeyboardInterrupt``, and other
        non-``Exception`` signals are not converted; they propagate and end
        the worker. A socket failure while sending propagates too.
        """
        try:
            result = self.execute(payload)
        except Exception as error:  # noqa: BLE001 - becomes this request's error
            self._send_result("error", error_data(error), request_id)
            return
        invalid = invalid_result(result, "Worker.execute")
        if invalid is not None:
            self._send_result("error", invalid, request_id)
            return
        try:
            self._send_result("done", result or {}, request_id)
        except (TypeError, ValueError) as error:
            # Encoding failed before any byte was written.
            self._send_result("error", error_data(error), request_id)

    def _dispatch(self, envelope: Envelope) -> None:
        """Dispatch one validated envelope to lifecycle or developer hooks."""
        operation = envelope["meta"].get("x_op")
        if operation == "terminate":
            self.close()
            return

        if operation == "execute":
            self._complete_execution(envelope["meta"]["x_req"], envelope["data"])
            return

        try:
            self.handle_envelope(envelope)
        except Exception as error:  # noqa: BLE001 - any hook failure is reported
            # No request to answer: an unsolicited error without ``x_req``.
            self._send_operation("error", error_data(error))

    def run(self) -> None:
        """Announce readiness, then receive and dispatch envelopes until
        termination or failure.

        The first frame sent is ``ready`` with this SDK's protocol version and
        capabilities. Returns normally after ``terminate``, after the Manager
        closes the connection, or after :meth:`close` from another thread. A
        malformed frame, invalid envelope, stalled frame, or socket failure
        raises the underlying exception. The connection is always closed
        first.

        Raises:
            ValueError: For protocol violations such as invalid JSON, an
                oversized frame, or a wrong-direction operation.
            OSError: For socket failures, including a stalled frame
                (``TimeoutError``) or a lost connection.
        """

        try:
            self._send_operation(
                "ready",
                {
                    "protocol_version": PROTOCOL_VERSION,
                    "capabilities": list(_CAPABILITIES),
                },
            )
            while self.__ok:
                try:
                    envelope = self._recv_envelope()
                except OSError:
                    if not self.__ok:
                        return
                    raise
                if envelope is None:
                    return
                self._dispatch(envelope)
        finally:
            self.close()
