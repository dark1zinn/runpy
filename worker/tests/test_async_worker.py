"""Behavior of the asyncio worker against an in-process Manager socket."""

import asyncio
import json
import struct
import sys
import tempfile

import pytest

from runpyrs import AsyncWorker, RunAsyncScript
from runpyrs.async_worker import CUSTOM_QUEUE_CAPACITY

TIMEOUT = 5


class Manager:
    """The Manager end of one worker connection."""

    def __init__(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        self.reader = reader
        self.writer = writer

    async def send(self, meta: dict, data: dict | None = None) -> None:
        body = json.dumps({"meta": meta, "data": data or {}}).encode()
        self.writer.write(struct.pack("<Q", len(body)) + body)
        await self.writer.drain()

    async def recv(self) -> dict:
        header = await asyncio.wait_for(self.reader.readexactly(8), TIMEOUT)
        (size,) = struct.unpack("<Q", header)
        return json.loads(await self.reader.readexactly(size))

    async def expect_closed(self) -> None:
        assert await asyncio.wait_for(self.reader.read(1), TIMEOUT) == b""


async def connect(worker_type: type) -> tuple[Manager, AsyncWorker, asyncio.Task, dict]:
    """Start ``worker_type`` against a fresh socket; return the ready frame."""
    directory = tempfile.mkdtemp()
    path = f"{directory}/worker.sock"
    accepted: asyncio.Future = asyncio.get_running_loop().create_future()

    async def on_connect(reader, writer):
        accepted.set_result(Manager(reader, writer))

    server = await asyncio.start_unix_server(on_connect, path)
    worker = worker_type(path, "worker-1")
    task = asyncio.ensure_future(worker.run())
    manager = await asyncio.wait_for(accepted, TIMEOUT)
    server.close()
    ready = await manager.recv()
    return manager, worker, task, ready


def run(coroutine) -> None:
    asyncio.run(asyncio.wait_for(coroutine, 20))


class Echo(AsyncWorker):
    async def execute(self, data):
        return {"echo": data}


def test_ready_advertises_cancellation_and_requests_are_correlated():
    async def scenario():
        manager, _, task, ready = await connect(Echo)
        assert ready["meta"]["x_op"] == "ready"
        assert ready["data"] == {
            "protocol_version": 1,
            "capabilities": ["requests", "cancel"],
        }

        await manager.send({"x_op": "execute", "x_req": "7"}, {"n": 1})
        done = await manager.recv()
        assert done["meta"]["x_op"] == "done"
        assert done["meta"]["x_req"] == "7"
        assert done["data"] == {"echo": {"n": 1}}

        await manager.send({"x_op": "terminate"})
        await asyncio.wait_for(task, TIMEOUT)
        await manager.expect_closed()

    run(scenario())


class Waiting(AsyncWorker):
    """Execute waits for a custom envelope, proving control stays responsive."""

    def __init__(self, *args):
        super().__init__(*args)
        self.released = asyncio.Event()
        self.started = asyncio.Event()
        self.cancelled = False

    async def execute(self, data):
        self.started.set()
        if data.get("forever"):
            try:
                await asyncio.sleep(3600)
            except asyncio.CancelledError:
                self.cancelled = True
                raise
        await self.released.wait()
        return {"released": True}

    async def handle_envelope(self, envelope):
        self.released.set()


def test_custom_envelopes_run_while_a_request_awaits():
    async def scenario():
        manager, _, task, _ = await connect(Waiting)
        await manager.send({"x_op": "execute", "x_req": "1"})
        await manager.send({"note": "release"}, {"go": True})
        done = await manager.recv()
        assert done["meta"]["x_req"] == "1"
        assert done["data"] == {"released": True}
        await manager.send({"x_op": "terminate"})
        await asyncio.wait_for(task, TIMEOUT)

    run(scenario())


def test_cancel_stops_the_active_request_with_one_terminal_answer():
    async def scenario():
        manager, worker, task, _ = await connect(Waiting)
        await manager.send({"x_op": "execute", "x_req": "1"}, {"forever": True})
        await asyncio.wait_for(worker.started.wait(), TIMEOUT)
        await manager.send({"x_op": "cancel", "x_req": "1"})
        answer = await manager.recv()
        assert answer["meta"] == {
            "x_op": "cancelled",
            "x_req": "1",
            "x_wid": "worker-1",
            "x_spath": answer["meta"]["x_spath"],
        }
        assert answer["data"] == {"status": "cancelled"}
        assert worker.cancelled

        # The worker is free again and no other answer for request 1 follows.
        await manager.send({"x_op": "execute", "x_req": "2"})
        await manager.send({"note": "release"})
        done = await manager.recv()
        assert (done["meta"]["x_op"], done["meta"]["x_req"]) == ("done", "2")
        await manager.send({"x_op": "terminate"})
        await asyncio.wait_for(task, TIMEOUT)

    run(scenario())


def test_cancel_right_behind_execute_gets_exactly_one_terminal_answer():
    async def scenario():
        manager, _, task, _ = await connect(Waiting)
        # Both frames arrive together; the cancel may be read before the
        # request task has run at all.
        await manager.send({"x_op": "execute", "x_req": "1"}, {"forever": True})
        await manager.send({"x_op": "cancel", "x_req": "1"})
        answer = await manager.recv()
        assert (answer["meta"]["x_op"], answer["meta"]["x_req"]) == ("cancelled", "1")
        assert answer["data"] == {"status": "cancelled"}

        # The worker accepts the next request, and nothing else follows for 1.
        await manager.send({"x_op": "execute", "x_req": "2"})
        await manager.send({"note": "release"})
        done = await manager.recv()
        assert (done["meta"]["x_op"], done["meta"]["x_req"]) == ("done", "2")
        await manager.send({"x_op": "terminate"})
        await asyncio.wait_for(task, TIMEOUT)

    run(scenario())


class Stubborn(AsyncWorker):
    def __init__(self, *args):
        super().__init__(*args)
        self.started = asyncio.Event()

    async def execute(self, data):
        self.started.set()
        try:
            await asyncio.sleep(3600)
        except asyncio.CancelledError:
            pass  # Suppress cancellation and finish normally.
        return {"finished": True}


def test_suppressed_cancellation_reports_the_result_then_already_finished():
    async def scenario():
        manager, worker, task, _ = await connect(Stubborn)
        await manager.send({"x_op": "execute", "x_req": "1"})
        await asyncio.wait_for(worker.started.wait(), TIMEOUT)
        await manager.send({"x_op": "cancel", "x_req": "1"})
        done = await manager.recv()
        assert (done["meta"]["x_op"], done["data"]) == ("done", {"finished": True})
        ack = await manager.recv()
        assert ack["meta"]["x_op"] == "cancelled"
        assert ack["meta"]["x_req"] == "1"
        assert ack["data"] == {"status": "already_finished"}
        await manager.send({"x_op": "terminate"})
        await asyncio.wait_for(task, TIMEOUT)

    run(scenario())


def test_cancel_for_a_request_that_is_not_running_is_acknowledged():
    async def scenario():
        manager, _, task, _ = await connect(Echo)
        await manager.send({"x_op": "execute", "x_req": "1"})
        assert (await manager.recv())["meta"]["x_op"] == "done"
        await manager.send({"x_op": "cancel", "x_req": "1"})
        ack = await manager.recv()
        assert ack["data"] == {"status": "already_finished"}
        await manager.send({"x_op": "terminate"})
        await asyncio.wait_for(task, TIMEOUT)

    run(scenario())


def test_terminate_cancels_the_active_request():
    async def scenario():
        manager, worker, task, _ = await connect(Waiting)
        await manager.send({"x_op": "execute", "x_req": "1"}, {"forever": True})
        await asyncio.wait_for(worker.started.wait(), TIMEOUT)
        await manager.send({"x_op": "terminate"})
        await asyncio.wait_for(task, TIMEOUT)
        assert worker.cancelled
        await manager.expect_closed()

    run(scenario())


@pytest.mark.parametrize(
    "meta",
    [
        {"x_op": "cancel"},
        {"x_op": "done", "x_req": "1"},
        {"x_op": "terminate", "x_req": "1"},
    ],
)
def test_protocol_violations_end_the_worker(meta):
    async def scenario():
        manager, _, task, _ = await connect(Echo)
        await manager.send(meta)
        with pytest.raises(ValueError):
            await asyncio.wait_for(task, TIMEOUT)
        await manager.expect_closed()

    run(scenario())


def test_second_execute_while_running_is_a_protocol_violation():
    async def scenario():
        manager, _, task, _ = await connect(Waiting)
        await manager.send({"x_op": "execute", "x_req": "1"}, {"forever": True})
        await manager.send({"x_op": "execute", "x_req": "2"})
        with pytest.raises(ValueError, match="still running"):
            await asyncio.wait_for(task, TIMEOUT)

    run(scenario())


class Blocked(AsyncWorker):
    started: asyncio.Event

    async def handle_envelope(self, envelope):
        Blocked.started.set()
        await asyncio.sleep(3600)


def test_full_custom_queue_reports_queue_full_and_control_stays_responsive():
    async def scenario():
        Blocked.started = asyncio.Event()
        manager, _, task, _ = await connect(Blocked)
        # One envelope occupies the handler, then exactly one overflows.
        await manager.send({"index": 0})
        await asyncio.wait_for(Blocked.started.wait(), TIMEOUT)
        for index in range(CUSTOM_QUEUE_CAPACITY + 1):
            await manager.send({"index": index + 1})
        error = await manager.recv()
        assert error["meta"]["x_op"] == "error"
        assert "x_req" not in error["meta"]
        assert error["data"]["kind"] == "QueueFull"

        await manager.send({"x_op": "cancel", "x_req": "9"})
        ack = await manager.recv()
        assert ack["data"] == {"status": "already_finished"}
        await manager.send({"x_op": "terminate"})
        await asyncio.wait_for(task, TIMEOUT)

    run(scenario())


def test_run_async_script_rejects_a_non_async_worker(monkeypatch, capsys):
    monkeypatch.setattr(sys, "argv", ["test", "/tmp/worker.sock", "worker-1"])
    with pytest.raises(SystemExit) as exit_info:
        RunAsyncScript(object)
    assert exit_info.value.code == 1
    assert "must inherit from AsyncWorker" in capsys.readouterr().err
