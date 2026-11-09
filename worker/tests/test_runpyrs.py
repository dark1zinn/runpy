import json
import socket
import struct
import sys
import tempfile
import threading
import time
from typing import get_args, get_origin, get_type_hints, Union

import pytest

from runpyrs import FrameTooLargeError
from runpyrs import (
    Data,
    Envelope,
    ExecutePayload,
    ExecuteResult,
    Meta,
    RunScript,
    Worker,
    create_envelope,
)


def _read_exact(stream: socket.socket, size: int) -> bytes:
    payload = bytearray()
    while len(payload) < size:
        chunk = stream.recv(size - len(payload))
        if not chunk:
            raise ConnectionError("connection closed")
        payload.extend(chunk)
    return bytes(payload)


def read_envelope(stream: socket.socket) -> dict:
    size = struct.unpack("<Q", _read_exact(stream, 8))[0]
    return json.loads(_read_exact(stream, size).decode("utf-8"))


def send_envelope(stream: socket.socket, envelope: object) -> None:
    payload = json.dumps(envelope, separators=(",", ":")).encode("utf-8")
    stream.sendall(struct.pack("<Q", len(payload)) + payload)


class EchoWorker(Worker):
    def execute(self, data: dict) -> dict:
        return {"echo": data}


def open_worker(worker_type: type[Worker] = EchoWorker):
    temp = tempfile.TemporaryDirectory()
    socket_path = f"{temp.name}/worker.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(socket_path)
    listener.listen(1)
    worker = worker_type(socket_path, "worker-123")
    connection, _ = listener.accept()
    connection.settimeout(1)
    ready = read_envelope(connection)
    return temp, listener, connection, worker, ready, socket_path


def close_worker(resources) -> None:
    temp, listener, connection, worker, _, _ = resources
    try:
        connection.close()
    finally:
        listener.close()
        worker.stream.close()
        temp.cleanup()


def test_envelope_types_describe_the_wire_shape():
    assert get_type_hints(Envelope) == {"meta": Meta, "data": Data}
    assert ExecutePayload is Data
    assert get_origin(ExecuteResult) is Union
    assert type(None) in get_args(ExecuteResult)
    assert issubclass(FrameTooLargeError, ValueError)


def test_create_envelope_preserves_json_metadata_and_copies_inputs():
    meta = {"some_custom_meta": 42}
    data = {"some": "data"}
    envelope = create_envelope(data, meta)
    meta["some_custom_meta"] = 0
    data["some"] = "changed"

    assert envelope == {
        "meta": {"some_custom_meta": 42},
        "data": {"some": "data"},
    }


@pytest.mark.parametrize("key", ["x_wid", "x_spath", "x_op", "x_custom"])
def test_create_envelope_rejects_reserved_metadata(key):
    with pytest.raises(ValueError, match="reserved by Runpy"):
        create_envelope({}, {key: "spoofed"})


@pytest.mark.parametrize("field", ["meta", "data"])
def test_create_envelope_requires_objects(field):
    kwargs = {"data": {}}
    if field == "data":
        kwargs["data"] = []
    else:
        kwargs["meta"] = []

    with pytest.raises(TypeError, match=f"{field} must be a dictionary"):
        create_envelope(**kwargs)


def test_worker_ready_envelope_uses_exact_argv_identity_and_socket():
    resources = open_worker()
    try:
        _, _, _, _, ready, socket_path = resources
        assert ready == {
            "meta": {
                "x_op": "ready",
                "x_wid": "worker-123",
                "x_spath": socket_path,
            },
            "data": {},
        }
    finally:
        close_worker(resources)


def test_custom_send_preserves_numeric_metadata():
    resources = open_worker()
    try:
        _, _, connection, worker, _, socket_path = resources
        worker.send({"some": "data"}, meta={"some_custom_meta": 42})
        assert read_envelope(connection) == {
            "meta": {
                "some_custom_meta": 42,
                "x_wid": "worker-123",
                "x_spath": socket_path,
            },
            "data": {"some": "data"},
        }
    finally:
        close_worker(resources)


def test_successful_log_does_not_write_stdout_fallback(capsys):
    resources = open_worker()
    try:
        _, _, connection, worker, _, _ = resources
        worker.log(
            {"message": "hello"},
            level="warning",
            meta={"level": "debug", "correlation_id": 7},
        )
        envelope = read_envelope(connection)
        assert envelope["meta"]["x_op"] == "log"
        assert envelope["meta"]["level"] == "warning"
        assert envelope["meta"]["correlation_id"] == 7
        assert envelope["data"] == {"message": "hello"}
        assert capsys.readouterr().out == ""
    finally:
        close_worker(resources)


def test_log_transport_failure_prints_flushed_fallback_and_reraises(monkeypatch):
    resources = open_worker()
    try:
        _, _, _, worker, _, _ = resources
        failure = BrokenPipeError("socket closed")
        printed = []

        def fail_send(*args, **kwargs):
            raise failure

        def record_print(*args, **kwargs):
            printed.append((args, kwargs))

        monkeypatch.setattr(worker, "_send_operation", fail_send)
        monkeypatch.setattr("builtins.print", record_print)
        with pytest.raises(BrokenPipeError) as raised:
            worker.log({"message": "lost"}, level="warning")

        assert raised.value is failure
        assert printed == [
            (
                ("[runpy-log-fallback][level=warning] {'message': 'lost'}",),
                {"flush": True},
            )
        ]
    finally:
        close_worker(resources)


@pytest.mark.parametrize(
    "print_error",
    [OSError("stdout closed"), ValueError("I/O operation on closed file")],
)
def test_log_fallback_failure_preserves_original_transport_error(
    monkeypatch, print_error
):
    resources = open_worker()
    try:
        _, _, _, worker, _, _ = resources
        failure = BrokenPipeError("socket closed")

        def fail_send(*args, **kwargs):
            raise failure

        def fail_print(*args, **kwargs):
            raise print_error

        monkeypatch.setattr(worker, "_send_operation", fail_send)
        monkeypatch.setattr("builtins.print", fail_print)
        with pytest.raises(BrokenPipeError) as raised:
            worker.log({"message": "lost"})

        assert raised.value is failure
    finally:
        close_worker(resources)


def test_log_serialization_error_does_not_print_fallback(capsys):
    resources = open_worker()
    try:
        _, _, _, worker, _, _ = resources
        with pytest.raises(TypeError):
            worker.log({"invalid": object()})
        assert capsys.readouterr().out == ""
    finally:
        close_worker(resources)


def test_execute_returns_direct_done_data():
    resources = open_worker()
    try:
        _, _, connection, worker, _, socket_path = resources
        thread = threading.Thread(target=worker.run)
        thread.start()

        send_envelope(
            connection,
            {
                "meta": {
                    "x_op": "execute",
                    "x_wid": "worker-123",
                    "x_spath": socket_path,
                },
                "data": {"value": 9},
            },
        )
        done = read_envelope(connection)
        assert done["meta"]["x_op"] == "done"
        assert done["data"] == {"echo": {"value": 9}}

        send_envelope(
            connection,
            {"meta": {"x_op": "terminate"}, "data": {}},
        )
        thread.join(timeout=1)
        assert not thread.is_alive()
    finally:
        close_worker(resources)


def test_custom_inbound_envelope_reaches_developer_hook_once():
    received = []
    called = threading.Event()

    class CustomWorker(Worker):
        def handle_envelope(self, envelope: Envelope) -> None:
            received.append(envelope)
            called.set()

    resources = open_worker(CustomWorker)
    try:
        _, _, connection, worker, _, _ = resources
        thread = threading.Thread(target=worker.run)
        thread.start()
        custom = {
            "meta": {
                "x_wid": "worker-123",
                "x_spath": "/tmp/manager.sock",
                "some_custom_meta": 42,
            },
            "data": {"some": "data"},
        }
        send_envelope(connection, custom)
        assert called.wait(timeout=1)
        assert received == [custom]

        send_envelope(
            connection,
            {"meta": {"x_op": "terminate"}, "data": {}},
        )
        thread.join(timeout=1)
    finally:
        close_worker(resources)


def test_retry_before_execute_returns_error():
    resources = open_worker()
    try:
        _, _, connection, worker, _, _ = resources
        thread = threading.Thread(target=worker.run)
        thread.start()
        send_envelope(connection, {"meta": {"x_op": "retry"}, "data": {}})
        error = read_envelope(connection)
        assert error["meta"]["x_op"] == "error"
        assert error["data"] == {
            "message": "Retry requested before an execute operation"
        }
        send_envelope(
            connection,
            {"meta": {"x_op": "terminate"}, "data": {}},
        )
        thread.join(timeout=1)
    finally:
        close_worker(resources)


def test_invalid_execute_result_returns_error():
    class InvalidResultWorker(Worker):
        def execute(self, data):
            return ["not", "an", "object"]

    resources = open_worker(InvalidResultWorker)
    try:
        _, _, connection, worker, _, _ = resources
        thread = threading.Thread(target=worker.run)
        thread.start()
        send_envelope(
            connection,
            {"meta": {"x_op": "execute"}, "data": {}},
        )
        error = read_envelope(connection)
        assert error["meta"]["x_op"] == "error"
        assert error["data"] == {
            "message": "Worker.execute must return a dictionary or None"
        }
        send_envelope(
            connection,
            {"meta": {"x_op": "terminate"}, "data": {}},
        )
        thread.join(timeout=1)
    finally:
        close_worker(resources)


@pytest.mark.parametrize(
    "invalid",
    [
        {"meta": {}, "data": []},
        {"meta": {"x_custom": "no"}, "data": {}},
        {"meta": {"x_op": 42}, "data": {}},
        {"meta": {"x_op": "ready"}, "data": {}},
    ],
)
def test_protocol_violation_closes_without_response(invalid):
    resources = open_worker()
    try:
        _, _, connection, worker, _, _ = resources
        thread = threading.Thread(target=worker.run)
        thread.start()
        send_envelope(connection, invalid)
        thread.join(timeout=1)
        assert not thread.is_alive()
        assert connection.recv(1) == b""
    finally:
        close_worker(resources)


def test_runscript_requires_socket_and_worker_id(monkeypatch):
    monkeypatch.setattr(sys, "argv", ["test"])
    with pytest.raises(SystemExit):
        RunScript(Worker)

    monkeypatch.setattr(sys, "argv", ["test", "/tmp/fake.sock"])
    with pytest.raises(SystemExit):
        RunScript(Worker)


def test_runscript_checks_subclass_with_complete_arguments(monkeypatch):
    monkeypatch.setattr(sys, "argv", ["test", "/tmp/fake.sock", "worker-123"])
    with pytest.raises(SystemExit):
        RunScript(object)


def test_runscript_passes_exact_worker_id(monkeypatch):
    captured = {}

    class CapturingWorker(Worker):
        def __init__(self, sock, name, extra):
            captured.update(sock=sock, name=name, extra=extra)

        def run(self):
            captured["ran"] = True

    monkeypatch.setattr(
        sys,
        "argv",
        ["test", "/tmp/worker.sock", "worker-123", "--mode=fast"],
    )
    RunScript(CapturingWorker)
    assert captured == {
        "sock": "/tmp/worker.sock",
        "name": "worker-123",
        "extra": {"mode": "fast"},
        "ran": True,
    }


def _frame(body: bytes) -> bytes:
    return struct.pack("<Q", len(body)) + body


@pytest.mark.parametrize(
    ("name", "value"),
    [
        ("RUNPY_MAX_FRAME_BYTES", "0"),
        ("RUNPY_MAX_FRAME_BYTES", "-1"),
        ("RUNPY_FRAME_TIMEOUT_MS", "soon"),
    ],
)
def test_invalid_manager_limits_fail_before_connecting(monkeypatch, name, value):
    monkeypatch.setenv(name, value)
    connected = []
    monkeypatch.setattr(
        socket.socket, "connect", lambda self, path: connected.append(path)
    )
    with pytest.raises(ValueError, match=name):
        Worker("/tmp/unused.sock", "worker-123")
    assert connected == []


def test_oversized_outbound_envelope_is_rejected_before_writing(monkeypatch):
    monkeypatch.setenv("RUNPY_MAX_FRAME_BYTES", "256")
    resources = open_worker()
    try:
        _, _, connection, worker, _, _ = resources
        with pytest.raises(FrameTooLargeError):
            worker.send({"blob": "x" * 1024})
        # Nothing was written, so the connection still carries valid frames.
        worker.send({"ok": True})
        assert read_envelope(connection)["data"] == {"ok": True}
    finally:
        close_worker(resources)


@pytest.mark.parametrize("size", [0, 257, 2**64 - 1])
def test_invalid_inbound_length_closes_without_reading_a_body(monkeypatch, size):
    monkeypatch.setenv("RUNPY_MAX_FRAME_BYTES", "256")
    resources = open_worker()
    try:
        _, _, connection, worker, _, _ = resources
        thread = threading.Thread(target=worker.run)
        thread.start()
        connection.sendall(struct.pack("<Q", size))
        thread.join(timeout=1)
        assert not thread.is_alive()
        assert connection.recv(1) == b""
    finally:
        close_worker(resources)


def test_frame_at_the_limit_is_accepted(monkeypatch):
    limit = 512
    prefix = '{"meta":{},"data":{"k":"'
    value = "v" * (limit - len(prefix) - 3)
    body = prefix + value + '"}}'
    assert len(body) == limit
    monkeypatch.setenv("RUNPY_MAX_FRAME_BYTES", str(limit))
    received = []

    class Recording(EchoWorker):
        def handle_envelope(self, envelope):
            received.append(envelope["data"])

    resources = open_worker(Recording)
    try:
        _, _, connection, worker, _, _ = resources
        thread = threading.Thread(target=worker.run)
        thread.start()
        connection.sendall(_frame(body.encode()))
        send_envelope(connection, {"meta": {"x_op": "terminate"}, "data": {}})
        thread.join(timeout=1)
        assert received == [{"k": value}]
    finally:
        close_worker(resources)


@pytest.mark.parametrize("sent", [3, 8 + 2])
def test_stalled_frame_times_out_after_its_first_byte(monkeypatch, sent):
    monkeypatch.setenv("RUNPY_FRAME_TIMEOUT_MS", "100")
    resources = open_worker()
    try:
        _, _, connection, worker, _, _ = resources
        thread = threading.Thread(target=worker.run)
        thread.start()
        connection.sendall(_frame(b'{"meta":{},"data":{}}')[:sent])
        thread.join(timeout=2)
        assert not thread.is_alive()
    finally:
        close_worker(resources)


def test_idle_connection_has_no_frame_deadline(monkeypatch):
    monkeypatch.setenv("RUNPY_FRAME_TIMEOUT_MS", "50")
    resources = open_worker()
    try:
        _, _, connection, worker, _, _ = resources
        thread = threading.Thread(target=worker.run)
        thread.start()
        time.sleep(0.3)
        assert thread.is_alive()
        send_envelope(connection, {"meta": {"x_op": "execute"}, "data": {"n": 1}})
        assert read_envelope(connection)["data"] == {"echo": {"n": 1}}
        send_envelope(connection, {"meta": {"x_op": "terminate"}, "data": {}})
        thread.join(timeout=1)
    finally:
        close_worker(resources)


def test_concurrent_sends_never_interleave_frames():
    resources = open_worker()
    try:
        _, _, connection, worker, _, _ = resources
        threads_count, per_thread = 8, 20
        received = []

        def reader():
            for _ in range(threads_count * per_thread):
                received.append(read_envelope(connection)["data"])

        connection.settimeout(10)
        reading = threading.Thread(target=reader)
        reading.start()

        def sender(index):
            for sequence in range(per_thread):
                worker.send(
                    {"thread": index, "sequence": sequence, "blob": str(index) * 65536}
                )

        senders = [
            threading.Thread(target=sender, args=(index,))
            for index in range(threads_count)
        ]
        for thread in senders:
            thread.start()
        for thread in senders:
            thread.join(timeout=10)
        reading.join(timeout=10)

        assert len(received) == threads_count * per_thread
        for data in received:
            assert data["blob"] == str(data["thread"]) * 65536
        for index in range(threads_count):
            sequences = [d["sequence"] for d in received if d["thread"] == index]
            assert sequences == list(range(per_thread))
    finally:
        close_worker(resources)


def test_ready_send_failure_closes_the_socket(monkeypatch):
    with tempfile.TemporaryDirectory() as directory:
        path = f"{directory}/worker.sock"
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(path)
        listener.listen(1)
        closed = []
        original_close = socket.socket.close

        def failing_sendall(self, data):
            raise BrokenPipeError("peer gone")

        def recording_close(self):
            closed.append(self)
            original_close(self)

        monkeypatch.setattr(socket.socket, "sendall", failing_sendall)
        monkeypatch.setattr(socket.socket, "close", recording_close)
        try:
            with pytest.raises(BrokenPipeError):
                EchoWorker(path, "worker-123")
            assert closed and all(sock.fileno() == -1 for sock in closed)
        finally:
            monkeypatch.undo()
            listener.close()


def test_failed_send_closes_the_transport_and_rejects_later_writes(monkeypatch):
    resources = open_worker()
    try:
        _, _, connection, worker, _, _ = resources
        calls = []

        def failing_sendall(self, data):
            calls.append(len(data))
            raise ConnectionResetError("reset mid-frame")

        monkeypatch.setattr(socket.socket, "sendall", failing_sendall)
        with pytest.raises(ConnectionResetError):
            worker.send({"first": True})
        with pytest.raises(ConnectionError):
            worker.send({"second": True})
        monkeypatch.undo()

        assert len(calls) == 1, "no write may follow a failed frame"
        assert connection.recv(1) == b""
    finally:
        close_worker(resources)


def test_non_finite_result_becomes_an_error_and_the_worker_stays_usable():
    class NaNWorker(Worker):
        def execute(self, data):
            if data.get("bad"):
                return {"value": float("nan")}
            return {"value": 1.5}

    resources = open_worker(NaNWorker)
    try:
        _, _, connection, worker, _, _ = resources
        thread = threading.Thread(target=worker.run)
        thread.start()
        send_envelope(connection, {"meta": {"x_op": "execute"}, "data": {"bad": True}})
        error = read_envelope(connection)
        assert error["meta"]["x_op"] == "error"
        assert "JSON compliant" in error["data"]["message"]

        send_envelope(connection, {"meta": {"x_op": "execute"}, "data": {}})
        assert read_envelope(connection)["data"] == {"value": 1.5}
        send_envelope(connection, {"meta": {"x_op": "terminate"}, "data": {}})
        thread.join(timeout=1)
        assert not thread.is_alive()
    finally:
        close_worker(resources)


def test_inbound_non_finite_number_is_a_protocol_violation():
    resources = open_worker()
    try:
        _, _, connection, worker, _, _ = resources
        thread = threading.Thread(target=worker.run)
        thread.start()
        connection.sendall(_frame(b'{"meta":{},"data":{"v":NaN}}'))
        thread.join(timeout=1)
        assert not thread.is_alive()
        assert connection.recv(1) == b""
    finally:
        close_worker(resources)


def test_close_is_idempotent_and_context_manager_closes():
    resources = open_worker()
    try:
        _, _, connection, worker, _, _ = resources
        with worker as entered:
            assert entered is worker
        assert worker.stream.fileno() == -1
        worker.close()
        with pytest.raises(ConnectionError):
            worker.send({"after": "close"})
        assert connection.recv(1) == b""
    finally:
        close_worker(resources)
