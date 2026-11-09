"""Managed worker connection, protocol validation, and dispatch lifecycle."""

import json
import socket
import threading
from types import TracebackType
from typing import Dict, Optional, Type

from ._protocol import (
    PROTOCOL_VERSION,
    FrameLimits,
    decode_body,
    encode_frame,
    recv_frame,
)
from .utils import (
    Data,
    Envelope,
    ExecutePayload,
    ExecuteResult,
    Meta,
    create_envelope,
)


_RUNPY_OPERATIONS = frozenset(
    {"ready", "execute", "retry", "terminate", "done", "error", "log"}
)
_MANAGER_OPERATIONS = frozenset({"execute", "retry", "terminate"})
_RESERVED_KEYS = frozenset({"x_wid", "x_spath", "x_op"})


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
        extra: Optional[Dict[str, str]] = None,
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
        self.__exec_payload: Optional[Data] = None
        self.__sock_path = sock
        self.name = name
        self.extra: Dict[str, str] = extra or {}

        self.stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            self.stream.connect(self.__sock_path)
            self.__ok = True
        except BaseException:
            self.close()
            raise

    def __enter__(self) -> "Worker":
        return self

    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc: Optional[BaseException],
        traceback: Optional[TracebackType],
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
        meta = dict(envelope["meta"])
        meta["x_wid"] = self.name
        meta["x_spath"] = self.__sock_path
        stamped: Envelope = {"meta": meta, "data": dict(envelope["data"])}
        self._send_bytes(encode_frame(stamped, self._limits.max_frame_bytes))

    def _send_operation(
        self,
        operation: str,
        data: Data,
        *,
        meta: Optional[Meta] = None,
    ) -> None:
        """Build and send one Python-to-Manager operation envelope."""
        envelope = create_envelope(data, meta)
        envelope["meta"]["x_op"] = operation
        self._send_envelope(envelope)

    def send(self, data: Data, *, meta: Optional[Meta] = None) -> None:
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
        meta: Optional[Meta] = None,
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
        """Process one ``execute`` or ``retry`` payload.

        Override this hook and return a dictionary for ``done.data`` or
        ``None`` for an empty result. Exceptions and non-dictionary results are
        converted to ``error`` envelopes; the receive loop remains active.
        """

        return None

    def _recv_envelope(self) -> Optional[Envelope]:
        """Read, decode, and validate one Manager-to-worker frame.

        The frame uses an 8-byte little-endian payload size followed by UTF-8
        JSON, bounded by the Manager-supplied size and partial-frame deadline.
        Invalid size, encoding, JSON, shape, metadata, or operation direction
        raises ``ValueError``; a stalled frame raises ``TimeoutError``.
        """
        payload = recv_frame(self.stream, self._limits)
        if payload is None:
            return None

        try:
            decoded = decode_body(payload)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ValueError(f"Invalid JSON envelope: {error}") from error

        return self._validate_inbound(decoded)

    @staticmethod
    def _validate_inbound(value: object) -> Envelope:
        """Validate the exact envelope shape and Manager operation boundary.

        Custom envelopes omit ``x_op``. Only ``execute``, ``retry``, and
        ``terminate`` are valid internal operations from Manager to worker.
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
            if key not in _RESERVED_KEYS:
                raise ValueError(f"Unknown Runpy metadata key '{key}'")
            if not isinstance(item, str):
                raise ValueError(f"Runpy metadata '{key}' must be a string")

        operation = meta.get("x_op")
        if operation is not None:
            if operation not in _RUNPY_OPERATIONS:
                raise ValueError(f"Unknown Runpy operation '{operation}'")
            if operation not in _MANAGER_OPERATIONS:
                raise ValueError(
                    f"Runpy operation '{operation}' is invalid for manager-to-worker envelopes"
                )

        return {"meta": dict(meta), "data": dict(data)}

    def _send_error(self, kind: str, message: str) -> None:
        """Send a structured ``error`` envelope with ``kind`` and ``message``."""
        self._send_operation("error", {"kind": kind, "message": message})

    def _send_exception(self, error: BaseException) -> None:
        """Report an application exception without losing its type.

        An exception with an empty message reports its type name as message.
        """
        kind = type(error).__name__
        self._send_error(kind, str(error) or kind)

    def _complete_execution(self, *, retry: bool = False) -> None:
        """Run the current execute payload and emit ``done`` or ``error``.

        Retry reuses the last payload. A retry before execute, an exception,
        an invalid result type, and a result that cannot be encoded produce
        structured ``error`` envelopes. ``SystemExit``, ``KeyboardInterrupt``,
        and other non-``Exception`` signals are not converted; they propagate
        and end the worker. A socket failure while sending propagates too.
        """
        if self.__exec_payload is None:
            self._send_error(
                "RuntimeError", "Retry requested before an execute operation"
            )
            return

        try:
            result = self.execute(self.__exec_payload)
        except Exception as error:
            self._send_exception(error)
            return
        if result is not None and not isinstance(result, dict):
            self._send_error(
                "TypeError", "Worker.execute must return a dictionary or None"
            )
            return
        try:
            self._send_operation("done", result or {})
        except (TypeError, ValueError) as error:
            # Encoding failed before any byte was written.
            self._send_exception(error)

    def _dispatch(self, envelope: Envelope) -> None:
        """Dispatch one validated envelope to lifecycle or developer hooks."""
        operation = envelope["meta"].get("x_op")
        if operation == "terminate":
            self.close()
            return

        if operation == "execute":
            self.__exec_payload = envelope["data"]
            self._complete_execution()
            return

        if operation == "retry":
            self._complete_execution(retry=True)
            return

        try:
            self.handle_envelope(envelope)
        except Exception as error:
            self._send_exception(error)

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
                {"protocol_version": PROTOCOL_VERSION, "capabilities": []},
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
